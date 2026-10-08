//! The workbench's own trigger sources, built from lash's primitives.
//!
//! Lash has no trigger API. The workbench keeps its subscriptions,
//! occurrences and deliveries in its own `<data-dir>/host-triggers.db`, and
//! each piece shows one primitive:
//!
//! - `workbench.register_trigger` is a host tool. It upserts one subscription
//!   keyed by the call's `call_id()`, so a redriven call registers once, and
//!   records the caller's session from `owner()`. Before it records anything
//!   it checks the input mapping against the definition's signature with the
//!   start-args check, and it pins the definition so it outlives the frame
//!   that created it.
//! - A source firing (a mail arrival, a `cron.Schedule` tick) records its
//!   occurrence and one delivery per matching subscription in one host
//!   transaction. After that commit the delivery pass starts each delivery's
//!   process under the host start key `{occurrence}:{subscription}` and binds
//!   the process id. A crash between the start and the bind is repaired by the
//!   same pass at the next boot: the key answers the process the first start
//!   made, so the occurrence still starts exactly one.
//! - Pruning contract: a start key deduplicates only while its process is
//!   retained. Delivered processes are host-originated, and the workbench
//!   prunes only processes a session originated, so it never prunes a
//!   process before its delivery is bound.
//!
//! The process-end notice ([`notice_pass`]) follows the process lifecycle
//! cursor and tells the subscribing session, once, that a delivered process
//! ended: its input id is `process-end:{process_id}`, so a retried send is
//! the same input.

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use lash::sync::RwLockExt;
use lash::tools::{
    ToolAttemptOutcome, ToolBinding, ToolCall, ToolContract, ToolDefinition,
    ToolDefinitionBindingExt, ToolManifest, ToolOutcome, ToolProvider,
};
use lash::{LashCore, ProcessId, SessionId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub(crate) const REGISTER_TRIGGER_TOOL_NAME: &str = "workbench_register_trigger";

/// The originator scope prefix of every process a delivery starts; the rest
/// of the scope is the subscribing session.
const DELIVERY_ORIGINATOR_PREFIX: &str = "workbench-trigger:";

/// The workbench's own sources: what a subscription watches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TriggerSource {
    /// Every mail delivered to any connected inbox; the event is a
    /// [`crate::mail::MailDelivery`].
    Mail,
    /// The ticks of a cron expression (an optional leading seconds field), read
    /// in `tz` (UTC when absent); the event is a [`CronTick`].
    Cron {
        expr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
}

impl TriggerSource {
    /// A value of the event this source delivers, for the registration check.
    fn sample_event(&self) -> Value {
        match self {
            Self::Mail => json!(crate::mail::MailDelivery {
                account: String::new(),
                title: String::new(),
                text: String::new(),
            }),
            Self::Cron { .. } => json!(CronTick {
                fired_at: String::new(),
            }),
        }
    }
}

/// The event one cron tick delivers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CronTick {
    /// The tick's own instant, RFC 3339.
    pub(crate) fired_at: String,
}

/// What `workbench.register_trigger` takes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterTrigger {
    source: TriggerSource,
    /// A definition from `processes.create`.
    definition: lash::process::ProcessDefinition,
    /// The argument the event is passed in.
    event_arg: String,
    /// The definition's other arguments, fixed at registration.
    #[serde(default)]
    args: Map<String, Value>,
    #[serde(default)]
    name: Option<String>,
}

/// One registration, as the triggers page lists it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Subscription {
    /// The registering call's id.
    pub(crate) id: String,
    pub(crate) owner_session: SessionId,
    pub(crate) name: Option<String>,
    pub(crate) source: TriggerSource,
    pub(crate) event_arg: String,
    pub(crate) args: Map<String, Value>,
    pub(crate) created_at_ms: i64,
    #[serde(skip)]
    definition: lash::process::ProcessDefinition,
    #[serde(skip)]
    pin: lash::process::HostArtifactPin,
}

/// One occurrence's delivery to one subscription that has no bound process
/// yet.
#[derive(Clone, Debug)]
pub(crate) struct Delivery {
    pub(crate) occurrence_id: String,
    pub(crate) subscription: Subscription,
    payload: Value,
}

impl Delivery {
    /// The host start key: one process per occurrence and subscription.
    pub(crate) fn start_key(&self) -> String {
        format!("{}:{}", self.occurrence_id, self.subscription.id)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HostTriggerError {
    #[error("trigger store lock is poisoned")]
    Poisoned,
    #[error("trigger store failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("trigger record does not encode: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("stored pin is malformed: {0}")]
    Pin(String),
    #[error("not an identity: {0}")]
    Identity(String),
    #[error("the workbench is not serving yet")]
    Unbound,
    #[error(transparent)]
    Lash(#[from] lash::EmbedError),
}

/// The workbench's trigger tables, and the core its tool and delivery pass
/// act on once the core is built.
#[derive(Clone)]
pub(crate) struct HostTriggers {
    connection: Arc<Mutex<Connection>>,
    core: Arc<RwLock<Option<LashCore>>>,
    /// Set when an occurrence was recorded; the delivery pass waits on it.
    recorded: Arc<tokio::sync::Notify>,
}

impl HostTriggers {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, HostTriggerError> {
        Self::from_connection(Connection::open(path)?)
    }

    pub(crate) fn in_memory() -> Result<Self, HostTriggerError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, HostTriggerError> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 15000;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS subscriptions (
               -- The registering call's id: a redriven call upserts this row.
               id TEXT PRIMARY KEY,
               owner_session TEXT NOT NULL,
               name TEXT,
               source_json TEXT NOT NULL,
               definition_json TEXT NOT NULL,
               event_arg TEXT NOT NULL,
               args_json TEXT NOT NULL,
               -- The host pin that holds the definition.
               pin TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS occurrences (
               id TEXT PRIMARY KEY,
               payload_json TEXT NOT NULL,
               recorded_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS deliveries (
               occurrence_id TEXT NOT NULL REFERENCES occurrences(id) ON DELETE CASCADE,
               subscription_id TEXT NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
               -- Bound once the delivery's process started.
               process_id TEXT,
               PRIMARY KEY (occurrence_id, subscription_id)
             );
             CREATE TABLE IF NOT EXISTS cursors (
               name TEXT PRIMARY KEY,
               position INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            core: Arc::new(RwLock::new(None)),
            recorded: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Act on `core` from now on: the tool's checks and the delivery pass.
    pub(crate) fn bind_core(&self, core: LashCore) {
        *self.core.write_recover() = Some(core);
    }

    /// Now on the core's clock, the one the cron timer reads.
    fn now_ms(&self) -> i64 {
        self.core
            .read_recover()
            .as_ref()
            .map_or_else(chrono::Utc::now, |core| {
                core.backend().clock().timestamp_datetime()
            })
            .timestamp_millis()
    }

    fn core(&self) -> Result<LashCore, HostTriggerError> {
        self.core
            .read_recover()
            .clone()
            .ok_or(HostTriggerError::Unbound)
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, HostTriggerError> {
        self.connection
            .lock()
            .map_err(|_| HostTriggerError::Poisoned)
    }

    pub(crate) fn provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(RegisterTriggerProvider {
            triggers: self.clone(),
        })
    }

    /// Check, record and pin one registration under `call_id`.
    async fn register(
        &self,
        call_id: &str,
        owner: &SessionId,
        input: RegisterTrigger,
    ) -> Result<String, String> {
        if let TriggerSource::Cron { expr, tz } = &input.source {
            crate::cron::schedule(expr, tz.as_deref())?;
        }
        let core = self.core().map_err(|error| error.to_string())?;
        let mut args = input.args.clone();
        args.insert(input.event_arg.clone(), input.source.sample_event());
        core.process_definitions()
            .check_args(&input.definition, &args, lash::process::ArgsMode::Complete)
            .await
            .map_err(|error| format!("the inputs do not fit the definition: {error}"))?;
        let pin = self
            .upsert(call_id, owner, &input)
            .map_err(|error| error.to_string())?;
        core.host_artifacts()
            .pin_definition(&pin, &input.definition.id)
            .await
            .map_err(|error| format!("the definition could not be held: {error}"))?;
        Ok(call_id.to_owned())
    }

    /// Insert the registration unless its call already did, and answer the
    /// pin the row holds.
    fn upsert(
        &self,
        call_id: &str,
        owner: &SessionId,
        input: &RegisterTrigger,
    ) -> Result<lash::process::HostArtifactPin, HostTriggerError> {
        let now_ms = self.now_ms();
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO subscriptions (
               id, owner_session, name, source_json, definition_json,
               event_arg, args_json, pin, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO NOTHING",
            params![
                call_id,
                owner.as_str(),
                input.name,
                serde_json::to_string(&input.source)?,
                serde_json::to_string(&input.definition)?,
                input.event_arg,
                serde_json::to_string(&input.args)?,
                lash::process::HostArtifactPin::mint().as_str(),
                now_ms,
            ],
        )?;
        let pin: String = connection.query_row(
            "SELECT pin FROM subscriptions WHERE id = ?1",
            [call_id],
            |row| row.get(0),
        )?;
        lash::process::HostArtifactPin::try_from(pin)
            .map_err(|error| HostTriggerError::Pin(error.to_string()))
    }

    /// Every registration, or `owner`'s.
    pub(crate) fn subscriptions(
        &self,
        owner: Option<&SessionId>,
    ) -> Result<Vec<Subscription>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, owner_session, name, source_json, definition_json,
                    event_arg, args_json, pin, created_at_ms
             FROM subscriptions
             WHERE ?1 IS NULL OR owner_session = ?1
             ORDER BY created_at_ms, id",
        )?;
        let rows = statement.query_map([owner.map(SessionId::as_str)], subscription_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Delete `owner`'s registration `id` and release its pin. Its unbound
    /// deliveries go with it; a process already started runs to its end.
    pub(crate) async fn delete(
        &self,
        owner: &SessionId,
        id: &str,
    ) -> Result<bool, HostTriggerError> {
        let removed = self
            .subscriptions(Some(owner))?
            .into_iter()
            .find(|subscription| subscription.id == id);
        let Some(removed) = removed else {
            return Ok(false);
        };
        self.remove(removed).await?;
        Ok(true)
    }

    /// Delete every registration `owner` made: its session is gone.
    pub(crate) async fn delete_owned_by(&self, owner: &SessionId) -> Result<(), HostTriggerError> {
        for subscription in self.subscriptions(Some(owner))? {
            self.remove(subscription).await?;
        }
        Ok(())
    }

    async fn remove(&self, subscription: Subscription) -> Result<(), HostTriggerError> {
        self.connection()?.execute(
            "DELETE FROM subscriptions WHERE id = ?1",
            [&subscription.id],
        )?;
        self.core()?
            .host_artifacts()
            .release(subscription.pin)
            .await?;
        Ok(())
    }

    /// Record a mail arrival under `occurrence_id` and notify the delivery
    /// pass.
    pub(crate) fn fire_mail(
        &self,
        occurrence_id: &str,
        delivery: &crate::mail::MailDelivery,
    ) -> Result<(), HostTriggerError> {
        self.record(occurrence_id, &json!(delivery), |source, _| {
            matches!(source, TriggerSource::Mail)
        })?;
        self.recorded.notify_one();
        Ok(())
    }

    /// Record `subscription`'s cron tick at `tick_ms` and notify the delivery
    /// pass.
    pub(crate) fn fire_cron_tick(
        &self,
        subscription: &str,
        tick: CronTick,
        tick_ms: u64,
    ) -> Result<(), HostTriggerError> {
        self.record(
            &format!("cron:{subscription}:{tick_ms}"),
            &json!(tick),
            |_, id| id == subscription,
        )?;
        self.recorded.notify_one();
        Ok(())
    }

    /// Record the occurrence `occurrence_id` and one delivery per matching
    /// subscription, in one transaction. A recorded occurrence is never
    /// recorded again, so a repeated firing delivers nothing new.
    pub(crate) fn record(
        &self,
        occurrence_id: &str,
        payload: &Value,
        matches: impl Fn(&TriggerSource, &str) -> bool,
    ) -> Result<(), HostTriggerError> {
        let subscriptions = self
            .subscriptions(None)?
            .into_iter()
            .filter(|subscription| matches(&subscription.source, &subscription.id))
            .map(|subscription| subscription.id)
            .collect::<Vec<_>>();
        let now_ms = self.now_ms();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let inserted = transaction.execute(
            "INSERT INTO occurrences (id, payload_json, recorded_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO NOTHING",
            params![occurrence_id, serde_json::to_string(payload)?, now_ms],
        )?;
        if inserted == 1 {
            for subscription in subscriptions {
                transaction.execute(
                    "INSERT INTO deliveries (occurrence_id, subscription_id, process_id)
                     VALUES (?1, ?2, NULL)",
                    params![occurrence_id, subscription],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every delivery whose process is not bound yet.
    pub(crate) fn unbound(&self) -> Result<Vec<Delivery>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT s.id, s.owner_session, s.name, s.source_json, s.definition_json,
                    s.event_arg, s.args_json, s.pin, s.created_at_ms,
                    d.occurrence_id, o.payload_json
             FROM deliveries d
             JOIN subscriptions s ON s.id = d.subscription_id
             JOIN occurrences o ON o.id = d.occurrence_id
             WHERE d.process_id IS NULL
             ORDER BY o.recorded_at_ms, d.occurrence_id, s.id",
        )?;
        let rows = statement.query_map([], |row| {
            let payload: String = row.get(10)?;
            Ok(Delivery {
                subscription: subscription_row(row)?,
                occurrence_id: row.get(9)?,
                payload: serde_json::from_str(&payload).map_err(|error| corrupt(10, error))?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The process bound to `occurrence_id`'s delivery to `subscription`.
    #[cfg(test)]
    pub(crate) fn bound_process(
        &self,
        occurrence_id: &str,
        subscription: &str,
    ) -> Result<Option<ProcessId>, HostTriggerError> {
        let process: Option<Option<String>> = self
            .connection()?
            .query_row(
                "SELECT process_id FROM deliveries
                 WHERE occurrence_id = ?1 AND subscription_id = ?2",
                params![occurrence_id, subscription],
                |row| row.get(0),
            )
            .optional()?;
        process
            .flatten()
            .map(|process| {
                ProcessId::parse(&process)
                    .map_err(|error| HostTriggerError::Identity(error.to_string()))
            })
            .transpose()
    }

    /// Start `delivery`'s process. A repeated start, after a crash or a lost
    /// answer, answers the process the first one started.
    pub(crate) async fn start(&self, delivery: &Delivery) -> Result<ProcessId, HostTriggerError> {
        let core = self.core()?;
        let subscription = &delivery.subscription;
        let mut args = subscription.args.clone();
        args.insert(subscription.event_arg.clone(), delivery.payload.clone());
        // The host decides what a delivered process runs with: the same
        // environment for every delivery, never the registering session's.
        let env_ref = core
            .host_artifacts()
            .publish_process_env(&subscription.pin, &delivered_process_environment())
            .await?;
        let lifetime = core
            .processes()
            .session_scope(&subscription.owner_session)
            .await?;
        let request = lash::process::ProcessStartRequest::new(
            lash::process::ProcessStartTarget::Definition {
                definition_id: subscription.definition.id.clone(),
                signature_claim: Some(subscription.definition.signature.clone()),
                args,
            },
            lash::process::ProcessOriginator::host_scoped(format!(
                "{DELIVERY_ORIGINATOR_PREFIX}{}",
                subscription.owner_session
            )),
            lash::process::Lifetime::Until(lifetime),
        )
        .with_env_ref(env_ref)
        .with_host_start_key(delivery.start_key());
        let operation = core
            .session_administration()
            .await
            .effect_host()
            .scoped(lash::runtime::AdmittedScope::runtime_operation(format!(
                "workbench-trigger-delivery:{}",
                delivery.start_key()
            )))
            .map_err(lash::EmbedError::from)?;
        let receipt = core.processes().start(request, operation).await?;
        Ok(receipt.process_id)
    }

    /// Bind `process_id` to `delivery`.
    pub(crate) fn bind(
        &self,
        delivery: &Delivery,
        process_id: &ProcessId,
    ) -> Result<(), HostTriggerError> {
        self.connection()?.execute(
            "UPDATE deliveries SET process_id = ?3
             WHERE occurrence_id = ?1 AND subscription_id = ?2 AND process_id IS NULL",
            params![
                delivery.occurrence_id,
                delivery.subscription.id,
                process_id.as_str()
            ],
        )?;
        Ok(())
    }

    /// Start and bind every unbound delivery; answers whether any failed.
    pub(crate) async fn deliver_unbound(&self) -> bool {
        let deliveries = match self.unbound() {
            Ok(deliveries) => deliveries,
            Err(error) => {
                eprintln!("agent-workbench triggers: the deliveries did not read: {error}");
                return true;
            }
        };
        let mut failed = false;
        for delivery in deliveries {
            let outcome = match self.start(&delivery).await {
                Ok(process_id) => self.bind(&delivery, &process_id),
                Err(error) => Err(error),
            };
            if let Err(error) = outcome {
                failed = true;
                eprintln!(
                    "agent-workbench triggers: delivery {} did not start: {error}",
                    delivery.start_key()
                );
            }
        }
        failed
    }

    fn cursor(&self, name: &str) -> Result<lash::process::ProcessChangeCursor, HostTriggerError> {
        let position: Option<i64> = self
            .connection()?
            .query_row(
                "SELECT position FROM cursors WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        Ok(position
            .and_then(|position| u64::try_from(position).ok())
            .map_or_else(
                lash::process::ProcessChangeCursor::initial,
                lash::process::ProcessChangeCursor::from_store_sequence,
            ))
    }

    fn commit_cursor(
        &self,
        name: &str,
        cursor: lash::process::ProcessChangeCursor,
    ) -> Result<(), HostTriggerError> {
        let position = i64::try_from(cursor.store_sequence()).unwrap_or(i64::MAX);
        self.connection()?.execute(
            "INSERT INTO cursors (name, position) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET position = excluded.position",
            params![name, position],
        )?;
        Ok(())
    }
}

fn corrupt(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

fn subscription_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Subscription> {
    let owner: String = row.get(1)?;
    let source: String = row.get(3)?;
    let definition: String = row.get(4)?;
    let args: String = row.get(6)?;
    let pin: String = row.get(7)?;
    Ok(Subscription {
        id: row.get(0)?,
        owner_session: SessionId::parse(owner).map_err(|error| corrupt(1, error))?,
        name: row.get(2)?,
        source: serde_json::from_str(&source).map_err(|error| corrupt(3, error))?,
        definition: serde_json::from_str(&definition).map_err(|error| corrupt(4, error))?,
        event_arg: row.get(5)?,
        args: serde_json::from_str(&args).map_err(|error| corrupt(6, error))?,
        pin: lash::process::HostArtifactPin::try_from(pin).map_err(|error| corrupt(7, error))?,
        created_at_ms: row.get(8)?,
    })
}

/// What every delivered process runs with: the host's choice, not the
/// registering session's environment.
fn delivered_process_environment() -> lash::process::ProcessExecutionEnvSpec {
    lash::process::ProcessExecutionEnvSpec::new(
        lash::plugins::AdmittedPluginConfig::default(),
        lash::runtime::SessionPolicy::new(
            lash::TurnBudget::bounded(32),
            lash::MaxToolCalls::new(1024),
        ),
    )
}

struct RegisterTriggerProvider {
    triggers: HostTriggers,
}

/// The `workbench.register_trigger` tool.
#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(crate) fn register_trigger_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:workbench_register_trigger",
        REGISTER_TRIGGER_TOOL_NAME,
        "Register a process definition to start on every event of a workbench source: \
         every mail delivered to a connected inbox, or each tick of a cron schedule. \
         The event is passed in `event_arg`; `args` fixes the definition's other arguments.",
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": { "kind": { "const": "mail" } },
                            "required": ["kind"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "kind": { "const": "cron" },
                                "expr": { "type": "string" },
                                "tz": { "type": "string" }
                            },
                            "required": ["kind", "expr"],
                            "additionalProperties": false
                        }
                    ]
                },
                "definition": {
                    "type": "object",
                    "description": "A process definition from `processes.create`."
                },
                "event_arg": { "type": "string" },
                "args": { "type": "object" },
                "name": { "type": "string" }
            },
            "required": ["source", "definition", "event_arg"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": { "subscription_id": { "type": "string" } },
            "required": ["subscription_id"],
            "additionalProperties": false
        }),
    )
    .expect("valid declared tool schemas")
    // A signature check and one row: a short body.
    .with_execution(Duration::from_secs(30))
    .with_tool_binding(ToolBinding::new(["workbench"], "register_trigger"))
}

#[async_trait]
impl ToolProvider for RegisterTriggerProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![register_trigger_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == REGISTER_TRIGGER_TOOL_NAME)
            .then(|| Arc::new(register_trigger_tool_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            if call.name() != REGISTER_TRIGGER_TOOL_NAME {
                return ToolOutcome::err_fmt(format_args!(
                    "unknown trigger tool `{}`",
                    call.name()
                ));
            }
            // A subscription belongs to the session that registered it: its
            // processes live until that session ends and report back to it.
            let owner = match call.context.session_id() {
                Ok(owner) => owner.clone(),
                Err(error) => return ToolOutcome::err_fmt(error),
            };
            let input = match RegisterTrigger::deserialize(call.args) {
                Ok(input) => input,
                Err(error) => return ToolOutcome::err_fmt(error),
            };
            match self
                .triggers
                .register(call.context.call_id().as_str(), &owner, input)
                .await
            {
                Ok(subscription_id) => {
                    ToolOutcome::ok(json!({ "subscription_id": subscription_id }))
                }
                Err(message) => ToolOutcome::err_fmt(message),
            }
        })
        .await
        .into()
    }
}

/// How often the delivery pass retries a failed start.
const DELIVERY_RETRY: Duration = Duration::from_secs(1);
/// How often the notice pass reads the lifecycle cursor.
const NOTICE_POLL: Duration = Duration::from_millis(250);
const NOTICE_CURSOR: &str = "process-end-notices";

/// The delivery and notice passes; [`Self::stop`] ends both.
#[derive(Clone)]
pub(crate) struct TriggerPasses {
    stop: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Default for TriggerPasses {
    fn default() -> Self {
        Self {
            stop: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }
}

impl TriggerPasses {
    /// Run both passes for `state`. The delivery pass's first run is the boot
    /// sweep: it starts and binds what a crash left unbound.
    pub(crate) fn start(&self, state: crate::AppState) {
        let mut stop = self.stop.subscribe();
        let triggers = state.host_triggers.clone();
        tokio::spawn(async move {
            loop {
                let failed = triggers.deliver_unbound().await;
                tokio::select! {
                    biased;
                    _ = stop.changed() => break,
                    () = triggers.recorded.notified() => {}
                    () = tokio::time::sleep(DELIVERY_RETRY), if failed => {}
                }
            }
        });
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            loop {
                let idle = match notice_pass(&state).await {
                    Ok(idle) => idle,
                    Err(error) => {
                        eprintln!("agent-workbench triggers: the notice pass stopped: {error}");
                        true
                    }
                };
                if idle {
                    tokio::select! {
                        biased;
                        _ = stop.changed() => break,
                        () = tokio::time::sleep(NOTICE_POLL) => {}
                    }
                }
            }
        });
    }

    pub(crate) fn stop(&self) {
        self.stop.send_replace(true);
    }
}

/// Read one page of process changes after the committed cursor, send a
/// process-end notice for every delivered process the page shows ended, and
/// commit the cursor. Answers whether the page was empty.
///
/// The cursor is committed only after every notice of the page was accepted.
/// A crash or a lost answer in between reads the page again and resends its
/// notices, and each resend is the same input: its id is the process's.
pub(crate) async fn notice_pass(state: &crate::AppState) -> Result<bool, HostTriggerError> {
    let triggers = &state.host_triggers;
    let cursor = triggers.cursor(NOTICE_CURSOR)?;
    let next = notify_ended_since(state, cursor).await?;
    if next == cursor {
        return Ok(true);
    }
    triggers.commit_cursor(NOTICE_CURSOR, next)?;
    Ok(false)
}

/// Send the notices of the page after `cursor`, and answer the page's end.
pub(crate) async fn notify_ended_since(
    state: &crate::AppState,
    cursor: lash::process::ProcessChangeCursor,
) -> Result<lash::process::ProcessChangeCursor, HostTriggerError> {
    let (changes, next) = state
        .core
        .process_registry()
        .processes_changed_since(cursor, 64)
        .await
        .map_err(lash::EmbedError::from)?;
    for change in changes {
        let lash::process::ProcessChange::Upsert { record } = change else {
            continue;
        };
        let status = record.status();
        if !status.is_terminal() {
            continue;
        }
        let lash::process::ProcessOriginator::Host { scope: Some(scope) } =
            &record.provenance.originator
        else {
            continue;
        };
        let Some(owner) = scope.strip_prefix(DELIVERY_ORIGINATOR_PREFIX) else {
            continue;
        };
        let owner = SessionId::parse(owner)
            .map_err(|error| HostTriggerError::Identity(error.to_string()))?;
        send_process_end_notice(state, &owner, &record.id, status).await?;
    }
    Ok(next)
}

/// Tell `owner` that the delivered process `process_id` ended in `status`.
/// The input's id is the process's, so this is one input however often it is
/// sent.
async fn send_process_end_notice(
    state: &crate::AppState,
    owner: &SessionId,
    process_id: &ProcessId,
    status: lash::process::ProcessStatus,
) -> Result<(), HostTriggerError> {
    let session = match state.open_session(owner, "triggers.process_end").await {
        Ok(session) => session,
        // A deleted session has nobody left to tell.
        Err(lash::EmbedError::UnknownSession { .. }) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let id = lash::TurnId::parse(format!("process-end:{process_id}"))
        .map_err(|error| HostTriggerError::Identity(error.to_string()))?;
    // The notice's run is one the page did not start: follow it.
    crate::turns::watch_session_runs(state, owner).await;
    session
        .send(lash::TurnInput::text(format!(
            "Triggered process {process_id} ended: {}.",
            json!(status).as_str().unwrap_or("ended")
        )))
        .id(id)
        .await?;
    state.trace_for_session(
        owner,
        "triggers.process_end_notice",
        json!({ "process_id": process_id, "status": status }),
    );
    Ok(())
}

impl crate::AppState {
    /// The registrations the triggers page lists for `session_id`.
    pub(crate) fn trigger_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<Subscription>, crate::AppError> {
        self.host_triggers
            .subscriptions(Some(session_id))
            .map_err(crate::AppError::internal)
    }
}
