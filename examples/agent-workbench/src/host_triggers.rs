//! The workbench's own trigger sources, built from lash's primitives.
//!
//! Lash has no trigger API. The workbench keeps its subscriptions,
//! occurrences and deliveries in its own `<data-dir>/host-triggers.db`, and
//! each piece shows one primitive:
//!
//! - `workbench.register_trigger` is a host tool. It checks the input mapping
//!   against the definition's signature with the start-args check, then
//!   records one subscription keyed by the call's `call_id()`, so a redriven
//!   call registers once, with the caller's session from `owner()`.
//! - A subscription has one lifecycle, and its row says where it is. It is
//!   recorded `pending` with a freshly minted host pin, the pin then takes
//!   hold of the definition, and only then is the row `pinned`. Only a
//!   `pinned` row is listed, ticked or delivered to. Removal marks the row
//!   `deleting`, which stops its dispatch in the same transaction, then
//!   releases the pin, then deletes the row. Both halves are finished by
//!   [`HostTriggers::recover`] at boot: a `pending` row is pinned or, if its
//!   definition is gone, removed; a `deleting` row's pin is released again
//!   (a release is idempotent) and the row deleted. So a subscription never
//!   dispatches a definition nothing holds, and a pin never outlives the row
//!   that names it.
//! - A session's delete removes its subscriptions as soon as its close is
//!   requested, before its tombstone. A workbench that dies in between
//!   leaves them to the same boot recovery, which removes every subscription
//!   whose session is no longer live.
//! - A source firing (a mail arrival, a `cron.Schedule` tick) records its
//!   occurrence and selects its recipients in one host transaction: one
//!   `INSERT ... SELECT` over the `pinned` subscriptions writes the
//!   deliveries, so a subscription removed at the same moment either has its
//!   delivery or does not, and never undoes the others'. A cron tick also
//!   advances its subscription's `ticked_through_ms` there, so a tick is
//!   recorded once however long its occurrence is kept.
//! - After that commit the delivery pass starts each delivery's process under
//!   the host start key `{occurrence}:{subscription}` and binds the process
//!   id. A crash between the start and the bind is repaired by the same pass
//!   at the next boot: the key answers the process the first start made, so
//!   the occurrence still starts exactly one. A start that fails is counted on
//!   the delivery; after [`DELIVERY_ATTEMPTS`] the delivery is recorded as
//!   failed with its last error and is not tried again.
//! - Retention: a start key deduplicates only while its process is retained,
//!   and a recorded occurrence is what answers a source that fires it again.
//!   The workbench never prunes a delivered process (it prunes only processes
//!   a session originated), so no process is pruned before its delivery is
//!   bound. An occurrence whose deliveries are all bound or failed is pruned,
//!   with them, once it is older than [`OCCURRENCE_RETENTION`]: longer than
//!   any workbench source can fire the same occurrence again.
//!
//! The process-end notice ([`notice_pass`]) follows the process lifecycle
//! cursor and tells the subscribing session, once, that a delivered process
//! ended: its input id is `process-end:{process_id}`, so a retried send is
//! the same input. A notice whose send keeps failing is counted, and after
//! [`NOTICE_ATTEMPTS`] it is recorded as skipped so the cursor moves on.

use std::collections::BTreeSet;
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
struct TriggerRegistrationInput {
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
    /// The last cron tick recorded for this registration.
    #[serde(skip)]
    pub(crate) ticked_through_ms: Option<i64>,
    #[serde(skip)]
    definition: lash::process::ProcessDefinition,
    #[serde(skip)]
    pin: lash::process::HostArtifactPin,
}

/// Where a subscription's row is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionState {
    /// Recorded; its pin does not hold the definition yet.
    Pending,
    /// Its pin holds the definition: the only state that dispatches.
    Pinned,
    /// Being removed; its pin is not released yet.
    Deleting,
}

impl SubscriptionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Pinned => "pinned",
            Self::Deleting => "deleting",
        }
    }
}

fn subscription_state(stored: &str) -> Result<SubscriptionState, HostTriggerError> {
    match stored {
        "pending" => Ok(SubscriptionState::Pending),
        "pinned" => Ok(SubscriptionState::Pinned),
        "deleting" => Ok(SubscriptionState::Deleting),
        other => Err(HostTriggerError::State(other.to_owned())),
    }
}

/// Which subscriptions an occurrence is delivered to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Recipients<'a> {
    /// Every subscription to the mail source.
    Mail,
    /// The cron subscription whose tick at `tick_ms` this is.
    CronTick { subscription: &'a str, tick_ms: u64 },
}

/// One occurrence's delivery to one subscription that is still due: no
/// process is bound to it and it has not failed.
#[derive(Clone, Debug)]
pub(crate) struct Delivery {
    pub(crate) occurrence_id: String,
    pub(crate) subscription: Subscription,
    payload: Value,
}

/// A delivery's row, as a law reads it.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RecordedDelivery {
    pub(crate) occurrence_id: String,
    pub(crate) subscription_id: String,
    /// The starts that failed.
    pub(crate) attempts: u32,
    /// The last error, once the delivery was given up.
    pub(crate) failure: Option<String>,
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
    #[error("stored subscription state `{0}` is unknown")]
    State(String),
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
               -- The registering call's id: a redriven call finds this row.
               id TEXT PRIMARY KEY,
               owner_session TEXT NOT NULL,
               name TEXT,
               source_json TEXT NOT NULL,
               definition_json TEXT NOT NULL,
               event_arg TEXT NOT NULL,
               args_json TEXT NOT NULL,
               -- The host pin that holds the definition once the row is pinned.
               pin TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL,
               -- The lifecycle: only a pinned row dispatches.
               state TEXT NOT NULL CHECK (state IN ('pending', 'pinned', 'deleting')),
               -- The last cron tick recorded for this row.
               ticked_through_ms INTEGER
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
               -- The starts that failed so far.
               attempts INTEGER NOT NULL DEFAULT 0,
               -- The last error, once the delivery was given up.
               failure TEXT,
               -- A delivery is due, bound or failed.
               CHECK (process_id IS NULL OR failure IS NULL),
               PRIMARY KEY (occurrence_id, subscription_id)
             );
             -- The process-end notices whose send failed.
             CREATE TABLE IF NOT EXISTS notices (
               process_id TEXT PRIMARY KEY,
               attempts INTEGER NOT NULL,
               error TEXT NOT NULL
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
        Arc::new(TriggerRegistrationProvider {
            triggers: self.clone(),
        })
    }

    /// Check and record one registration under `call_id`, then take hold of
    /// its definition. The registration exists once its row is `pinned`.
    async fn register(
        &self,
        call_id: &str,
        owner: &SessionId,
        input: TriggerRegistrationInput,
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
        let (pin, state) = self
            .insert_pending(call_id, owner, &input)
            .map_err(|error| error.to_string())?;
        match state {
            // A redriven call whose first run finished.
            SubscriptionState::Pinned => return Ok(call_id.to_owned()),
            SubscriptionState::Deleting => {
                return Err(format!("registration {call_id} is being removed"));
            }
            SubscriptionState::Pending => {}
        }
        if let Err(error) = core
            .host_artifacts()
            .pin_definition(&pin, &input.definition.id)
            .await
        {
            // Nothing holds the definition, so the registration never was:
            // remove the row and end its pin.
            self.abandon(call_id).await;
            return Err(format!("the definition could not be held: {error}"));
        }
        if self
            .mark_pinned(call_id)
            .map_err(|error| error.to_string())?
        {
            Ok(call_id.to_owned())
        } else {
            Err(format!(
                "registration {call_id} was removed while it was made"
            ))
        }
    }

    /// Insert the registration as `pending` unless its call already did, and
    /// answer the pin and state its row holds.
    fn insert_pending(
        &self,
        call_id: &str,
        owner: &SessionId,
        input: &TriggerRegistrationInput,
    ) -> Result<(lash::process::HostArtifactPin, SubscriptionState), HostTriggerError> {
        let now_ms = self.now_ms();
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO subscriptions (
               id, owner_session, name, source_json, definition_json,
               event_arg, args_json, pin, created_at_ms, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending')
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
        let (pin, state): (String, String) = connection.query_row(
            "SELECT pin, state FROM subscriptions WHERE id = ?1",
            [call_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((
            lash::process::HostArtifactPin::try_from(pin)
                .map_err(|error| HostTriggerError::Pin(error.to_string()))?,
            subscription_state(&state)?,
        ))
    }

    /// Move `id` from `pending` to `pinned`, and answer whether it is pinned.
    fn mark_pinned(&self, id: &str) -> Result<bool, HostTriggerError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE subscriptions SET state = 'pinned' WHERE id = ?1 AND state = 'pending'",
            [id],
        )?;
        let state: Option<String> = connection
            .query_row(
                "SELECT state FROM subscriptions WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(state.as_deref() == Some(SubscriptionState::Pinned.as_str()))
    }

    /// Every active registration, or `owner`'s: the `pinned` rows.
    pub(crate) fn subscriptions(
        &self,
        owner: Option<&SessionId>,
    ) -> Result<Vec<Subscription>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, owner_session, name, source_json, definition_json,
                    event_arg, args_json, pin, created_at_ms, ticked_through_ms
             FROM subscriptions
             WHERE state = 'pinned' AND (?1 IS NULL OR owner_session = ?1)
             ORDER BY created_at_ms, id",
        )?;
        let rows = statement.query_map([owner.map(SessionId::as_str)], subscription_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Every row's id and lifecycle state, in id order.
    #[cfg(test)]
    pub(crate) fn lifecycle(&self) -> Result<Vec<(String, SubscriptionState)>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT id, state FROM subscriptions ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(id, state)| Ok((id, subscription_state(&state)?)))
            .collect()
    }

    /// Put `id`'s row in `state`, as a workbench that died there left it.
    #[cfg(test)]
    pub(crate) fn leave_in(
        &self,
        id: &str,
        state: SubscriptionState,
    ) -> Result<(), HostTriggerError> {
        self.connection()?.execute(
            "UPDATE subscriptions SET state = ?2 WHERE id = ?1",
            params![id, state.as_str()],
        )?;
        Ok(())
    }

    /// The pin and definition `id`'s row names, whatever its state.
    #[cfg(test)]
    pub(crate) fn held(
        &self,
        id: &str,
    ) -> Result<
        Option<(
            lash::process::HostArtifactPin,
            lash::process::ProcessDefinition,
        )>,
        HostTriggerError,
    > {
        Ok([
            SubscriptionState::Pending,
            SubscriptionState::Pinned,
            SubscriptionState::Deleting,
        ]
        .into_iter()
        .map(|state| self.in_state(state))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .find(|(row, _, _)| row == id)
        .map(|(_, pin, definition)| (pin, definition)))
    }

    /// Delete `owner`'s registration `id` and release its pin. Its due
    /// deliveries go with it; a process already started runs to its end.
    pub(crate) async fn delete(
        &self,
        owner: &SessionId,
        id: &str,
    ) -> Result<bool, HostTriggerError> {
        if self.mark_deleting(Some(id), Some(owner))? == 0 {
            return Ok(false);
        }
        self.finish_removals().await;
        Ok(true)
    }

    /// Delete every registration `owner` made: its session is going.
    pub(crate) async fn delete_owned_by(&self, owner: &SessionId) -> Result<(), HostTriggerError> {
        if self.mark_deleting(None, Some(owner))? > 0 {
            self.finish_removals().await;
        }
        Ok(())
    }

    /// Remove the registration `id` whose definition could not be held.
    async fn abandon(&self, id: &str) {
        match self.mark_deleting(Some(id), None) {
            Ok(_) => self.finish_removals().await,
            Err(error) => {
                eprintln!("agent-workbench triggers: registration {id} was not removed: {error}");
            }
        }
    }

    /// The first half of a removal, in one transaction: mark the rows `id`
    /// and `owner` select as `deleting` and drop their deliveries, so they
    /// stop dispatching here. Answers how many rows are marked.
    pub(crate) fn mark_deleting(
        &self,
        id: Option<&str>,
        owner: Option<&SessionId>,
    ) -> Result<usize, HostTriggerError> {
        let selected = "(?1 IS NULL OR id = ?1) AND (?2 IS NULL OR owner_session = ?2)";
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let marked = transaction.execute(
            &format!("UPDATE subscriptions SET state = 'deleting' WHERE {selected}"),
            params![id, owner.map(SessionId::as_str)],
        )?;
        transaction.execute(
            &format!(
                "DELETE FROM deliveries WHERE subscription_id IN (
                   SELECT id FROM subscriptions WHERE {selected}
                 )"
            ),
            params![id, owner.map(SessionId::as_str)],
        )?;
        transaction.commit()?;
        Ok(marked)
    }

    /// The second half of every removal: release each `deleting` row's pin,
    /// then delete the row. A release that fails leaves its row `deleting`
    /// for the next call; releasing a pin again changes nothing.
    pub(crate) async fn finish_removals(&self) {
        let outcome = async {
            let core = self.core()?;
            for (id, pin, _) in self.in_state(SubscriptionState::Deleting)? {
                match core.host_artifacts().release(pin).await {
                    Ok(()) => {
                        self.connection()?.execute(
                            "DELETE FROM subscriptions WHERE id = ?1 AND state = 'deleting'",
                            [&id],
                        )?;
                    }
                    Err(error) => eprintln!(
                        "agent-workbench triggers: registration {id}'s pin was not released: {error}"
                    ),
                }
            }
            Ok::<_, HostTriggerError>(())
        }
        .await;
        if let Err(error) = outcome {
            eprintln!("agent-workbench triggers: removals did not finish: {error}");
        }
    }

    /// The id, pin and definition of every row in `state`.
    fn in_state(
        &self,
        state: SubscriptionState,
    ) -> Result<
        Vec<(
            String,
            lash::process::HostArtifactPin,
            lash::process::ProcessDefinition,
        )>,
        HostTriggerError,
    > {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, pin, definition_json FROM subscriptions WHERE state = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([state.as_str()], |row| {
            let pin: String = row.get(1)?;
            let definition: String = row.get(2)?;
            Ok((
                row.get(0)?,
                lash::process::HostArtifactPin::try_from(pin).map_err(|error| corrupt(1, error))?,
                serde_json::from_str(&definition).map_err(|error| corrupt(2, error))?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Finish what a crash left half done, before anything dispatches:
    ///
    /// - a `pending` row's pin takes hold of its definition and the row is
    ///   `pinned`; if the definition cannot be held any more, nobody was told
    ///   the registration exists, so it is removed;
    /// - every row whose session is no longer live is removed;
    /// - every `deleting` row's pin is released and the row deleted.
    pub(crate) async fn recover(&self) -> Result<(), HostTriggerError> {
        let core = self.core()?;
        for (id, pin, definition) in self.in_state(SubscriptionState::Pending)? {
            match core
                .host_artifacts()
                .pin_definition(&pin, &definition.id)
                .await
            {
                Ok(()) => {
                    self.mark_pinned(&id)?;
                }
                Err(error) => {
                    eprintln!(
                        "agent-workbench triggers: registration {id}'s definition could not be held: {error}"
                    );
                    self.mark_deleting(Some(&id), None)?;
                }
            }
        }
        let owners = self.owners()?;
        if !owners.is_empty() {
            // Read after the rows: an owner that is not listed was deleted.
            let live = core
                .sessions_filtered(lash::SessionListFilter {
                    deleted: Some(false),
                    ..Default::default()
                })
                .await?
                .into_iter()
                .map(|view| view.session_id)
                .collect::<BTreeSet<_>>();
            for owner in owners.difference(&live) {
                self.mark_deleting(None, Some(owner))?;
            }
        }
        self.finish_removals().await;
        Ok(())
    }

    /// The sessions that own a row.
    fn owners(&self) -> Result<BTreeSet<SessionId>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT DISTINCT owner_session FROM subscriptions")?;
        let rows = statement.query_map([], |row| {
            let owner: String = row.get(0)?;
            SessionId::parse(owner).map_err(|error| corrupt(0, error))
        })?;
        rows.collect::<Result<BTreeSet<_>, _>>().map_err(Into::into)
    }

    /// Record a mail arrival under `occurrence_id` and notify the delivery
    /// pass.
    pub(crate) fn fire_mail(
        &self,
        occurrence_id: &str,
        delivery: &crate::mail::MailDelivery,
    ) -> Result<(), HostTriggerError> {
        self.record(occurrence_id, &json!(delivery), Recipients::Mail)?;
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
            Recipients::CronTick {
                subscription,
                tick_ms,
            },
        )?;
        self.recorded.notify_one();
        Ok(())
    }

    /// Record the occurrence `occurrence_id` and one delivery per recipient,
    /// in one transaction that also selects the recipients: the `pinned`
    /// subscriptions `recipients` names. A recorded occurrence is never
    /// recorded again, so a repeated firing delivers nothing new.
    pub(crate) fn record(
        &self,
        occurrence_id: &str,
        payload: &Value,
        recipients: Recipients<'_>,
    ) -> Result<(), HostTriggerError> {
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
            match recipients {
                Recipients::Mail => {
                    transaction.execute(
                        "INSERT INTO deliveries (occurrence_id, subscription_id)
                         SELECT ?1, id FROM subscriptions
                         WHERE state = 'pinned'
                           AND json_extract(source_json, '$.kind') = 'mail'",
                        [occurrence_id],
                    )?;
                }
                Recipients::CronTick {
                    subscription,
                    tick_ms,
                } => {
                    transaction.execute(
                        "INSERT INTO deliveries (occurrence_id, subscription_id)
                         SELECT ?1, id FROM subscriptions
                         WHERE state = 'pinned' AND id = ?2",
                        params![occurrence_id, subscription],
                    )?;
                    transaction.execute(
                        "UPDATE subscriptions SET ticked_through_ms = ?2
                         WHERE state = 'pinned' AND id = ?1",
                        params![subscription, i64::try_from(tick_ms).unwrap_or(i64::MAX)],
                    )?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every due delivery: no process is bound to it, it has not failed, and
    /// its subscription is `pinned`.
    pub(crate) fn unbound(&self) -> Result<Vec<Delivery>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT s.id, s.owner_session, s.name, s.source_json, s.definition_json,
                    s.event_arg, s.args_json, s.pin, s.created_at_ms, s.ticked_through_ms,
                    d.occurrence_id, o.payload_json
             FROM deliveries d
             JOIN subscriptions s ON s.id = d.subscription_id
             JOIN occurrences o ON o.id = d.occurrence_id
             WHERE d.process_id IS NULL AND d.failure IS NULL AND s.state = 'pinned'
             ORDER BY o.recorded_at_ms, d.occurrence_id, s.id",
        )?;
        let rows = statement.query_map([], |row| {
            let payload: String = row.get(11)?;
            Ok(Delivery {
                subscription: subscription_row(row)?,
                occurrence_id: row.get(10)?,
                payload: serde_json::from_str(&payload).map_err(|error| corrupt(11, error))?,
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

    /// Every recorded delivery, by occurrence and subscription.
    #[cfg(test)]
    pub(crate) fn deliveries(&self) -> Result<Vec<RecordedDelivery>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT occurrence_id, subscription_id, attempts, failure FROM deliveries
             ORDER BY occurrence_id, subscription_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(RecordedDelivery {
                occurrence_id: row.get(0)?,
                subscription_id: row.get(1)?,
                attempts: row.get(2)?,
                failure: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The ids of the recorded occurrences.
    #[cfg(test)]
    pub(crate) fn occurrences(&self) -> Result<Vec<String>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT id FROM occurrences ORDER BY id")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
             WHERE occurrence_id = ?1 AND subscription_id = ?2
               AND process_id IS NULL AND failure IS NULL",
            params![
                delivery.occurrence_id,
                delivery.subscription.id,
                process_id.as_str()
            ],
        )?;
        Ok(())
    }

    /// Count a failed start of `delivery` and, at [`DELIVERY_ATTEMPTS`],
    /// record the delivery as failed with `error`. Answers whether the
    /// delivery is still due.
    fn record_failed_start(
        &self,
        delivery: &Delivery,
        error: &str,
    ) -> Result<bool, HostTriggerError> {
        let due: Option<bool> = self
            .connection()?
            .query_row(
                "UPDATE deliveries
                 SET attempts = attempts + 1,
                     failure = CASE WHEN attempts + 1 >= ?3 THEN ?4 END
                 WHERE occurrence_id = ?1 AND subscription_id = ?2
                   AND process_id IS NULL AND failure IS NULL
                 RETURNING failure IS NULL",
                params![
                    delivery.occurrence_id,
                    delivery.subscription.id,
                    DELIVERY_ATTEMPTS,
                    error
                ],
                |row| row.get(0),
            )
            .optional()?;
        // No row: the delivery went with its subscription.
        Ok(due.unwrap_or(false))
    }

    /// Start and bind every due delivery; answers whether any is still due
    /// and should be tried again.
    pub(crate) async fn deliver_unbound(&self) -> bool {
        let deliveries = match self.unbound() {
            Ok(deliveries) => deliveries,
            Err(error) => {
                eprintln!("agent-workbench triggers: the deliveries did not read: {error}");
                return true;
            }
        };
        let mut retry = false;
        for delivery in deliveries {
            let process_id = match self.start(&delivery).await {
                Ok(process_id) => process_id,
                Err(error) => {
                    let message = error.to_string();
                    // A count that was not written is one more attempt.
                    let due = self
                        .record_failed_start(&delivery, &message)
                        .unwrap_or(true);
                    retry |= due;
                    eprintln!(
                        "agent-workbench triggers: delivery {} did not start{}: {message}",
                        delivery.start_key(),
                        if due { "" } else { " and is given up" }
                    );
                    continue;
                }
            };
            // A lost bind leaves the delivery due: the next start answers the
            // same process.
            if let Err(error) = self.bind(&delivery, &process_id) {
                retry = true;
                eprintln!(
                    "agent-workbench triggers: delivery {} was not bound: {error}",
                    delivery.start_key()
                );
            }
        }
        retry
    }

    /// Delete every occurrence recorded [`OCCURRENCE_RETENTION`] ago or
    /// earlier that has no due delivery, and its deliveries with it. Answers
    /// how many occurrences went.
    pub(crate) fn prune_settled(&self) -> Result<usize, HostTriggerError> {
        let horizon_ms = self
            .now_ms()
            .saturating_sub(i64::try_from(OCCURRENCE_RETENTION.as_millis()).unwrap_or(i64::MAX));
        Ok(self.connection()?.execute(
            "DELETE FROM occurrences
             WHERE recorded_at_ms <= ?1
               AND NOT EXISTS (
                 SELECT 1 FROM deliveries d
                 WHERE d.occurrence_id = occurrences.id
                   AND d.process_id IS NULL AND d.failure IS NULL
               )",
            [horizon_ms],
        )?)
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

    /// Whether `process_id`'s notice was given up.
    fn notice_skipped(&self, process_id: &ProcessId) -> Result<bool, HostTriggerError> {
        let attempts: Option<u32> = self
            .connection()?
            .query_row(
                "SELECT attempts FROM notices WHERE process_id = ?1",
                [process_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(attempts.is_some_and(|attempts| attempts >= NOTICE_ATTEMPTS))
    }

    /// Count a failed send of `process_id`'s notice with its `error`, and
    /// answer whether the notice is now given up.
    fn record_failed_notice(
        &self,
        process_id: &ProcessId,
        error: &str,
    ) -> Result<bool, HostTriggerError> {
        let attempts: u32 = self.connection()?.query_row(
            "INSERT INTO notices (process_id, attempts, error) VALUES (?1, 1, ?2)
             ON CONFLICT(process_id) DO UPDATE
               SET attempts = attempts + 1, error = excluded.error
             RETURNING attempts",
            params![process_id.as_str(), error],
            |row| row.get(0),
        )?;
        Ok(attempts >= NOTICE_ATTEMPTS)
    }

    /// Forget the failed sends of a notice that was accepted.
    fn notice_sent(&self, process_id: &ProcessId) -> Result<(), HostTriggerError> {
        self.connection()?.execute(
            "DELETE FROM notices WHERE process_id = ?1",
            [process_id.as_str()],
        )?;
        Ok(())
    }

    /// The process-end notices that were given up, with their last error.
    #[cfg(test)]
    pub(crate) fn skipped_notices(&self) -> Result<Vec<(String, String)>, HostTriggerError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT process_id, error FROM notices WHERE attempts >= ?1 ORDER BY process_id",
        )?;
        let rows = statement.query_map([NOTICE_ATTEMPTS], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
        ticked_through_ms: row.get(9)?,
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
            lash::NoProgressBudget::bounded(12),
        ),
    )
}

struct TriggerRegistrationProvider {
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
impl ToolProvider for TriggerRegistrationProvider {
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
            let input = match TriggerRegistrationInput::deserialize(call.args) {
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
/// How many starts a delivery gets before it is recorded as failed.
pub(crate) const DELIVERY_ATTEMPTS: u32 = 5;
/// How many sends a process-end notice gets before it is skipped.
pub(crate) const NOTICE_ATTEMPTS: u32 = 5;
/// How long a settled occurrence is kept. The mail source can fire an
/// occurrence again only while the sending call is redriven, which its
/// execution bound ends within minutes, and a cron tick is never fired again
/// once its subscription is ticked through it.
pub(crate) const OCCURRENCE_RETENTION: Duration = Duration::from_secs(60 * 60);
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
    /// Run both passes for `state`. The delivery pass begins with the boot
    /// recovery, which finishes the registrations and removals a crash left
    /// half done, and its first run starts and binds what a crash left
    /// unbound. Every run also finishes the removals a failed release left
    /// and prunes the settled occurrences.
    pub(crate) fn start(&self, state: crate::AppState) {
        let mut stop = self.stop.subscribe();
        let triggers = state.host_triggers.clone();
        tokio::spawn(async move {
            if let Err(error) = triggers.recover().await {
                eprintln!("agent-workbench triggers: the boot recovery stopped: {error}");
            }
            loop {
                triggers.finish_removals().await;
                let retry = triggers.deliver_unbound().await;
                if let Err(error) = triggers.prune_settled() {
                    eprintln!("agent-workbench triggers: the occurrences were not pruned: {error}");
                }
                tokio::select! {
                    biased;
                    _ = stop.changed() => break,
                    () = triggers.recorded.notified() => {}
                    () = tokio::time::sleep(DELIVERY_RETRY), if retry => {}
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
/// The cursor is committed only after every notice of the page was accepted
/// or given up. A crash or a lost answer in between reads the page again and
/// resends its notices, and each resend is the same input: its id is the
/// process's. A send that fails is counted and ends the pass, so the page is
/// read again; at [`NOTICE_ATTEMPTS`] the notice is recorded as skipped and
/// the page goes on without it.
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
        let triggers = &state.host_triggers;
        if triggers.notice_skipped(&record.id)? {
            continue;
        }
        let sent = match SessionId::parse(owner) {
            Ok(owner) => send_process_end_notice(state, &owner, &record.id, status).await,
            Err(error) => Err(HostTriggerError::Identity(error.to_string())),
        };
        match sent {
            Ok(()) => triggers.notice_sent(&record.id)?,
            Err(error) => {
                if !triggers.record_failed_notice(&record.id, &error.to_string())? {
                    return Err(error);
                }
                eprintln!(
                    "agent-workbench triggers: the end notice of {} is skipped: {error}",
                    record.id
                );
            }
        }
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
    pub(crate) fn host_trigger_registrations(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<Subscription>, crate::AppError> {
        self.host_triggers
            .subscriptions(Some(session_id))
            .map_err(crate::AppError::internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process-end notice whose send keeps failing is given up at
    /// [`NOTICE_ATTEMPTS`] and stays given up; an accepted send forgets the
    /// failures before it.
    #[test]
    fn a_notice_is_skipped_after_its_attempts_and_a_sent_one_forgets_its_failures() {
        let triggers = HostTriggers::in_memory().expect("open the trigger tables");
        let failing = ProcessId::fixture("failing");
        let recovering = ProcessId::fixture("recovering");
        for attempt in 1..NOTICE_ATTEMPTS {
            assert!(
                !triggers
                    .record_failed_notice(&failing, "refused")
                    .expect("count the failure"),
                "attempt {attempt} is tried again"
            );
            assert!(!triggers.notice_skipped(&failing).expect("read the notice"));
        }
        triggers
            .record_failed_notice(&recovering, "refused")
            .expect("count the failure");
        triggers
            .notice_sent(&recovering)
            .expect("forget the failure");
        assert!(
            triggers
                .record_failed_notice(&failing, "refused for good")
                .expect("count the failure"),
            "the last attempt gives the notice up"
        );
        assert!(triggers.notice_skipped(&failing).expect("read the notice"));
        assert_eq!(
            triggers
                .skipped_notices()
                .expect("read the skipped notices"),
            vec![(failing.to_string(), "refused for good".to_string())]
        );
    }
}
