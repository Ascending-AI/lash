//! The facade retires the runtime-operation scope it mints for every plugin
//! task once the receipt — or the failure — is back, through the same
//! `EffectJournalRetirement` lever hosts reach for process and session
//! journals (FIG-2499, FIG-2500). A runtime operation the facade did not mint
//! is left alone.

use lash_sansio::SessionId;
use std::sync::{Arc, Mutex};

use lash::durability::{EffectHost, EffectJournalRetirement};
use lash::plugins::{
    PluginCommand, PluginError, PluginFactory, PluginOperation, PluginOperationFailure,
    PluginRegistrar, PluginSessionContext, PluginTask, PluginTaskContext, SessionParam,
    SessionPlugin,
};
use lash::{LashCore, PluginBinding};
use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, EffectAddress, ExecutionScope, Resolution,
    RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome,
};
use lash_sansio::sync::MutexExt;
use lash_sqlite_store::SqliteEffectHost;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const IN_FLIGHT_OPERATION: &str = "in-flight-runtime-operation";

/// What a task left behind under its facade-minted scope: the scope's journal
/// key and the promise it minted there, so the test can prove both existed
/// before the receipt and are fenced after it.
#[derive(Clone, Default)]
struct Minted(Arc<Mutex<Vec<(String, AwaitEventKey)>>>);

#[derive(Clone)]
struct JournalConfig {
    host: Arc<dyn EffectHost>,
    minted: Minted,
}

struct JournalPlugin;

impl PluginBinding for JournalPlugin {
    const ID: &'static str = "journal_task";
    type SessionConfig = JournalConfig;

    fn factory(config: &Self::SessionConfig) -> Arc<dyn PluginFactory> {
        Arc::new(JournalFactory {
            config: config.clone(),
        })
    }
}

struct JournalFactory {
    config: JournalConfig,
}

impl PluginFactory for JournalFactory {
    fn id(&self) -> &'static str {
        JournalPlugin::ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(JournalSessionPlugin {
            config: self.config.clone(),
        }))
    }
}

struct JournalSessionPlugin {
    config: JournalConfig,
}

struct JournalTaskOp;

impl PluginOperation for JournalTaskOp {
    const NAME: &'static str = "journal.record";
    const DESCRIPTION: &'static str = "journal one effect and one promise, then succeed";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
}
impl PluginTask for JournalTaskOp {}

struct FailingTaskOp;

struct JournalCommandOp;

impl PluginOperation for JournalCommandOp {
    const NAME: &'static str = "journal.command";
    const DESCRIPTION: &'static str = "a command whose facade-minted scope retires at receipt";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
}
impl PluginCommand for JournalCommandOp {}

struct DrainingTaskOp;

impl PluginOperation for DrainingTaskOp {
    const NAME: &'static str = "journal.drain";
    const DESCRIPTION: &'static str = "return while a run-to-completion loser is still draining";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
}
impl PluginTask for DrainingTaskOp {}

impl PluginOperation for FailingTaskOp {
    const NAME: &'static str = "journal.fail";
    const DESCRIPTION: &'static str = "journal one effect and one promise, then fail";
    const SESSION_PARAM: SessionParam = SessionParam::Optional;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
}
impl PluginTask for FailingTaskOp {}

impl SessionPlugin for JournalSessionPlugin {
    fn id(&self) -> &'static str {
        JournalPlugin::ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let config = self.config.clone();
        reg.operations()
            .typed_task_value::<JournalTaskOp, _, _>(move |ctx, _args| {
                let config = config.clone();
                async move {
                    let scope_key = journal_under_operation_scope(&config, &ctx).await?;
                    Ok(json!({ "scope": scope_key }))
                }
            })?;
        let config = self.config.clone();
        reg.operations()
            .typed_task_value::<FailingTaskOp, _, _>(move |ctx, _args| {
                let config = config.clone();
                async move {
                    journal_under_operation_scope(&config, &ctx).await?;
                    Err(PluginOperationFailure::new(
                        "the task fails after journaling",
                    ))
                }
            })?;
        reg.operations()
            .typed_command_value::<JournalCommandOp, _, _>(|_ctx, _args| async {
                Ok(json!({ "ok": true }))
            })?;
        reg.operations()
            .typed_task_value::<DrainingTaskOp, _, _>(|ctx, args| async move {
                let gate = drain_gate(args["gate"].as_str().expect("the task names its gate"));
                let scope = ctx.scoped_effect_controller.execution_scope().clone();
                let scope_key = scope
                    .journal_identity()
                    .map_err(|err| PluginOperationFailure::new(err.to_string()))?
                    .key()
                    .to_string();
                let attribution = ctx
                    .session_id
                    .clone()
                    .map(RuntimeAttribution::for_session)
                    .unwrap_or_else(RuntimeAttribution::none);
                let controller = ctx.scoped_effect_controller.controller();
                let group = lash_core::RuntimeEffectGroup::try_new(
                    lash_core::RuntimeEffectInvocation::new(
                        EffectAddress::new(scope.clone(), format!("{scope_key}:drain-group"))
                            .expect("drain group carries an admitted effect scope"),
                        attribution.clone(),
                        "drain-group",
                    ),
                    format!("{scope_key}:drain-group"),
                    vec![
                        envelope(&scope, "fast-child", attribution.clone()),
                        envelope(&scope, "draining-child", attribution),
                    ],
                    lash_core::GroupWakePolicy::First,
                    lash_core::LoserPolicy::RunToCompletion,
                )
                .map_err(|err| PluginOperationFailure::new(err.to_string()))?;
                let mut handle = controller
                    .open_effect_group(group)
                    .await
                    .map_err(|err| PluginOperationFailure::new(err.to_string()))?;
                let first = controller
                    .await_next_settlement(&mut handle, CancellationToken::new())
                    .await
                    .map_err(|err| PluginOperationFailure::new(err.to_string()))?;
                assert_eq!(first.position, 0, "the fast child wins");
                // The loser is already running when the group closes: the
                // close hands it to the drain instead of cancelling it.
                gate.started.notified().await;
                controller
                    .close_effect_group(handle, lash_core::LoserPolicy::RunToCompletion)
                    .await
                    .map_err(|err| PluginOperationFailure::new(err.to_string()))?;
                Ok(json!({ "scope": scope_key }))
            })
    }
}

/// The draining child's gate: it reports when it starts and blocks until the
/// test releases it, so the receipt returns while the child is still live.
struct DrainGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// One gate per test run, named so backends running in parallel in this
/// binary never share a gate.
fn drain_gate(name: &str) -> Arc<DrainGate> {
    static GATES: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Arc<DrainGate>>>> =
        std::sync::OnceLock::new();
    GATES
        .get_or_init(Default::default)
        .lock_recover()
        .entry(name.to_string())
        .or_insert_with(|| {
            Arc::new(DrainGate {
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            })
        })
        .clone()
}

/// Group executors for a durable host: the draining child waits on the named
/// gate, everything else settles at once.
struct DrainExecutors {
    gate: String,
}

impl lash_core::GroupExecutors for DrainExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let draining = matches!(
            &envelope.command,
            RuntimeEffectCommand::LanguageRuntimeValue { operation } if operation == "draining-child"
        );
        Some(if draining {
            let gate = drain_gate(&self.gate);
            RuntimeEffectLocalExecutor::testing(move |_| {
                let gate = Arc::clone(&gate);
                async move {
                    gate.started.notify_one();
                    gate.release.notified().await;
                    Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                        value: json!({ "drained": true }),
                    })
                }
            })
        } else {
            executor()
        })
    }
}

/// Journal one effect and mint-and-resolve one promise under the task's
/// facade-minted runtime-operation scope; return that scope's journal key.
async fn journal_under_operation_scope(
    config: &JournalConfig,
    ctx: &PluginTaskContext,
) -> Result<String, PluginOperationFailure> {
    let scope = ctx.scoped_effect_controller.execution_scope().clone();
    let scope_key = scope
        .journal_identity()
        .map_err(|err| PluginOperationFailure::new(err.to_string()))?
        .key()
        .to_string();
    ctx.scoped_effect_controller
        .controller()
        .execute_effect(
            envelope(
                &scope,
                "task-effect",
                ctx.session_id
                    .clone()
                    .map(RuntimeAttribution::for_session)
                    .unwrap_or_else(RuntimeAttribution::none),
            ),
            executor(),
        )
        .await
        .map_err(|err| PluginOperationFailure::new(err.to_string()))?;
    let key = config
        .host
        .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("task-call"))
        .await
        .map_err(|err| PluginOperationFailure::new(err.to_string()))?;
    config
        .host
        .resolve_await_event(&key, Resolution::Ok(json!("answered")))
        .await
        .map_err(|err| PluginOperationFailure::new(err.to_string()))?;
    let terminal = config
        .host
        .peek_await_event(&key)
        .await
        .map_err(|err| PluginOperationFailure::new(err.to_string()))?;
    assert_eq!(
        terminal,
        Some(Resolution::Ok(json!("answered"))),
        "the promise row exists under the operation scope while the task runs"
    );
    config
        .minted
        .0
        .lock_recover()
        .push((scope_key.clone(), key));
    Ok(scope_key)
}

fn envelope(
    scope: &ExecutionScope,
    effect_id: &str,
    attribution: RuntimeAttribution,
) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), effect_id)
                .expect("runtime-operation effect carries an admitted scope"),
            attribution,
            effect_id,
        ),
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: effect_id.to_string(),
        },
    )
}

fn executor() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async {
        Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
            value: json!({"journaled": true}),
        })
    })
}

fn core_with_host(effect_host: Arc<dyn EffectHost>) -> LashCore {
    core_with_host_and_store(
        effect_host,
        Some(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        )),
    )
}

/// A core over `effect_host`; with a store factory the session's receipts
/// persist, which is what the reclaim sweep reads as its proof.
fn core_with_host_and_store(
    effect_host: Arc<dyn EffectHost>,
    store_factory: Option<Arc<dyn lash::persistence::SessionStoreFactory>>,
) -> LashCore {
    let provider = lash_core::testing::TestProvider::builder()
        .complete(|_request| async {
            Ok(lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::Text {
                    text: "unused".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..lash::provider::LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let builder = LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(provider)
        .model(
            lash::ModelSpec::builder("mock-model")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        );
    let builder = match store_factory {
        Some(store_factory) => builder.store_factory(store_factory),
        None => builder,
    };
    builder
        .effect_host(effect_host)
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "op-retirement-test-worker",
            "op-retirement-test-boot",
        ))
        .expect("core")
}

#[tokio::test]
async fn plugin_task_scopes_retire_after_their_receipt_and_leave_other_operations_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("runtime-operation-retirement.db");
    let host: Arc<dyn EffectHost> = Arc::new(
        SqliteEffectHost::open(&path)
            .await
            .expect("SQLite effect host"),
    );

    // A runtime operation the facade did not mint: nothing here may touch it.
    let in_flight = ExecutionScope::runtime_operation(IN_FLIGHT_OPERATION);
    host.scoped(in_flight.clone())
        .expect("in-flight scope binds")
        .controller()
        .execute_effect(
            envelope(&in_flight, "in-flight-effect", RuntimeAttribution::none()),
            executor(),
        )
        .await
        .expect("in-flight operation journals");
    let in_flight_key = in_flight
        .journal_identity()
        .expect("runtime-operation journal identity")
        .key()
        .to_string();

    let minted = Minted::default();
    let core = core_with_host(Arc::clone(&host));
    let session = core
        .session("op-retirement")
        .plugin::<JournalPlugin>(JournalConfig {
            host: Arc::clone(&host),
            minted: minted.clone(),
        })
        .open()
        .await
        .expect("session");
    let operations = session.plugin_operations();

    let receipt = operations
        .run_task::<JournalTaskOp>(json!({}))
        .await
        .expect("the succeeding task returns its receipt");
    let succeeded_scope = receipt.output["scope"]
        .as_str()
        .expect("the receipt names its scope")
        .to_string();
    operations
        .run_task::<FailingTaskOp>(json!({}))
        .await
        .expect_err("the failing task surfaces its failure");

    let minted = minted.0.lock_recover().clone();
    assert_eq!(
        minted.len(),
        2,
        "both tasks journaled under their own scope"
    );
    assert_eq!(minted[0].0, succeeded_scope);
    assert_ne!(minted[0].0, minted[1].0, "every task gets a fresh scope");

    let conn = rusqlite::Connection::open(&path).expect("open the effect journal");
    let count = |sql: &str, scope_id: &str| -> i64 {
        conn.query_row(sql, [scope_id], |row| row.get(0))
            .expect("count rows")
    };
    let effects = "SELECT COUNT(*) FROM runtime_effect_replay WHERE scope_id = ?1";
    let promises = "SELECT COUNT(*) FROM await_event_waits WHERE scope_json = ?1";
    let fences = "SELECT COUNT(*) FROM effect_scope_retirements WHERE scope_id = ?1";
    for (scope_key, key) in &minted {
        let scope_json = serde_json::to_string(&ExecutionScope::runtime_operation(
            scope_key_operation_id(scope_key),
        ))
        .expect("scope json");
        assert_eq!(count(effects, scope_key), 0, "task effect rows are retired");
        assert_eq!(
            count(promises, &scope_json),
            0,
            "task promise rows are retired"
        );
        assert_eq!(count(fences, scope_key), 1, "the task scope is fenced");
        let error = host
            .peek_await_event(key)
            .await
            .expect_err("a retired task's promise no longer reads");
        assert_eq!(error.code.as_str(), "await_event_unknown_or_revoked");
    }
    assert_eq!(
        count(effects, &in_flight_key),
        1,
        "the in-flight operation keeps its row"
    );
    assert_eq!(
        count(fences, &in_flight_key),
        0,
        "the in-flight operation is not fenced"
    );

    // The same lever hosts reach: retiring the in-flight operation themselves.
    let deleted = host
        .retire_effect_journal(EffectJournalRetirement::runtime_operation(
            IN_FLIGHT_OPERATION,
        ))
        .await
        .expect("hosts retire runtime operations through the journal lever");
    assert_eq!(deleted, 1);
    assert_eq!(count(effects, &in_flight_key), 0);
    assert_eq!(count(fences, &in_flight_key), 1);
}

/// The operation id inside a runtime-operation journal key.
fn scope_key_operation_id(scope_key: &str) -> String {
    let key: serde_json::Value = serde_json::from_str(scope_key).expect("journal key is JSON");
    key["execution_id"]
        .as_str()
        .expect("runtime-operation journal keys carry their operation id")
        .to_string()
}

/// A plugin command's facade-minted scope retires at receipt exactly like a
/// task's: one fence per command invocation, named for the command.
#[tokio::test]
async fn plugin_command_scopes_retire_after_their_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("command-retirement.db");
    let host: Arc<dyn EffectHost> = Arc::new(
        SqliteEffectHost::open(&path)
            .await
            .expect("SQLite effect host"),
    );
    let core = core_with_host(Arc::clone(&host));
    let session = core
        .session("command-retirement")
        .plugin::<JournalPlugin>(JournalConfig {
            host: Arc::clone(&host),
            minted: Minted::default(),
        })
        .open()
        .await
        .expect("session");
    let operations = session.plugin_operations();
    let first = operations
        .run_command::<JournalCommandOp>(json!({}))
        .await
        .expect("the command returns its receipt");
    assert_eq!(first.output, json!({ "ok": true }));
    let second = operations
        .run_command::<JournalCommandOp>(json!({}))
        .await
        .expect("a repeated command returns a fresh receipt");
    assert_eq!(second.output, json!({ "ok": true }));

    let conn = rusqlite::Connection::open(&path).expect("open the effect journal");
    let fences: Vec<String> = conn
        .prepare("SELECT scope_id FROM effect_scope_retirements ORDER BY scope_id")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("fence rows");
    assert_eq!(
        fences.len(),
        2,
        "every command invocation retires its own scope"
    );
    assert_ne!(fences[0], fences[1]);
    for fence in &fences {
        assert!(
            scope_key_operation_id(fence).contains(":plugin_command:journal.command:"),
            "the fence names the command scope: {fence}"
        );
    }
}

/// The receipt and its observations are recorded before retirement runs, and
/// a retirement failure is logged rather than turned into an operation
/// failure (review round 1): the task's work is not lost because its reclaim failed.
#[tokio::test]
async fn plugin_task_receipt_stands_when_scope_retirement_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("retirement-failure.db");
    let inner: Arc<dyn EffectHost> = Arc::new(
        SqliteEffectHost::open(&path)
            .await
            .expect("SQLite effect host"),
    );
    let host: Arc<dyn EffectHost> = Arc::new(RetirementFailsHost {
        inner: Arc::clone(&inner),
    });
    let minted = Minted::default();
    let core = core_with_host(Arc::clone(&host));
    let session = core
        .session("retirement-failure")
        .plugin::<JournalPlugin>(JournalConfig {
            host: Arc::clone(&inner),
            minted: minted.clone(),
        })
        .open()
        .await
        .expect("session");
    let receipt = session
        .plugin_operations()
        .run_task::<JournalTaskOp>(json!({}))
        .await
        .expect("the receipt stands even though retirement failed");
    let scope_key = receipt.output["scope"]
        .as_str()
        .expect("the receipt names its scope")
        .to_string();
    assert_eq!(minted.0.lock_recover().len(), 1);

    let conn = rusqlite::Connection::open(&path).expect("open the effect journal");
    let count = |sql: &str| -> i64 {
        conn.query_row(sql, [&scope_key], |row| row.get(0))
            .expect("count rows")
    };
    assert_eq!(
        count("SELECT COUNT(*) FROM runtime_effect_replay WHERE scope_id = ?1"),
        1,
        "a failed retirement leaves the journal rows for a later reclaim"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM effect_scope_retirements WHERE scope_id = ?1"),
        0,
        "a failed retirement fences nothing"
    );
}

/// An effect host whose scope retirement always fails; everything else is the
/// SQLite host underneath.
struct RetirementFailsHost {
    inner: Arc<dyn EffectHost>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for RetirementFailsHost {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, lash_core::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, lash_core::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, lash_core::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl EffectHost for RetirementFailsHost {
    fn turn_control_binding_id(&self) -> String {
        "retirement-fails-host".to_string()
    }

    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.inner.scoped(scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.inner.scoped_static(scope)
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    async fn retire_effect_journal(
        &self,
        _retirement: EffectJournalRetirement,
    ) -> Result<usize, lash_core::RuntimeError> {
        Err(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::SqliteEffectJournalRetirement,
            "injected retirement failure",
        ))
    }
}

/// The durable journal a sweep test observes: row counts by scope key.
enum Journal {
    Sqlite(std::path::PathBuf),
    Postgres(sqlx::PgPool),
}

impl Journal {
    async fn count(&self, table: &str, scope_key: &str, extra: &str) -> i64 {
        match self {
            Journal::Sqlite(path) => rusqlite::Connection::open(path)
                .expect("open the effect journal")
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE scope_id = ?1 {extra}"),
                    [scope_key],
                    |row| row.get(0),
                )
                .expect("count rows"),
            Journal::Postgres(pool) => sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM lash_{table} WHERE scope_id = $1 {extra}"
            ))
            .bind(scope_key)
            .fetch_one(pool)
            .await
            .expect("count rows"),
        }
    }

    async fn effects(&self, scope_key: &str) -> i64 {
        self.count("runtime_effect_replay", scope_key, "").await
    }

    async fn in_progress(&self, scope_key: &str) -> i64 {
        self.count(
            "runtime_effect_replay",
            scope_key,
            "AND status = 'in_progress'",
        )
        .await
    }

    async fn fences(&self, scope_key: &str) -> i64 {
        self.count("effect_scope_retirements", scope_key, "").await
    }
}

/// A task that returns while a run-to-completion loser is still draining
/// keeps its journal: the receipt is not proof that the scope is quiescent,
/// so the facade leaves the scope alone and the reclaim sweep — the durable
/// owner of deferred retirement (ADR 0067) — retires it once the recorded
/// receipt is joined by quiescence. The sweep refuses while the loser is
/// live, survives the facade and session going away, and never touches a
/// scope without a recorded receipt (FIG-2499 fix round 2, ruling 2).
async fn draining_task_is_retired_by_the_reclaim_sweep(pg: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate = format!("drain-{}", if pg { "postgres" } else { "sqlite" });
    let mut postgres = None;
    let (host, journal, store_factory): (
        Arc<dyn EffectHost>,
        Journal,
        Arc<dyn lash::persistence::SessionStoreFactory>,
    ) = if pg {
        let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
            assert!(
                std::env::var("LASH_REQUIRE_POSTGRES").is_err(),
                "LASH_REQUIRE_POSTGRES=1 but LASH_POSTGRES_DATABASE_URL is not set"
            );
            eprintln!(
                "skipping Postgres sweep retirement test: LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        };
        let admin = sqlx::PgPool::connect(&url).await.expect("connect postgres");
        let name = format!("sweep_retirement_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create a private database");
        admin.close().await;
        let (base, _) = url.rsplit_once('/').expect("database url has a path");
        let storage = lash_postgres_store::PostgresStorage::connect(&format!("{base}/{name}"))
            .await
            .expect("connect the private database");
        let host = storage.effect_host();
        host.register_group_executors(Arc::new(DrainExecutors { gate: gate.clone() }))
            .expect("register group executors");
        let factory = storage.session_store_factory_with_shared_process_registry();
        let pool = storage.pool().clone();
        postgres = Some(storage);
        (Arc::new(host), Journal::Postgres(pool), Arc::new(factory))
    } else {
        let path = dir.path().join("drain-retirement.db");
        let host = SqliteEffectHost::open(&path)
            .await
            .expect("SQLite effect host");
        host.register_group_executors(Arc::new(DrainExecutors { gate: gate.clone() }))
            .expect("register group executors");
        let factory =
            lash_sqlite_store::SqliteSessionStoreFactory::new(dir.path().join("sessions"));
        (Arc::new(host), Journal::Sqlite(path), Arc::new(factory))
    };
    let _postgres = postgres.take();

    // A runtime operation nobody recorded a receipt for: the sweep has no
    // proof it is unreachable and must leave it alone.
    let in_flight = ExecutionScope::runtime_operation(format!("{gate}-in-flight"));
    host.scoped(in_flight.clone())
        .expect("in-flight scope binds")
        .controller()
        .execute_effect(
            envelope(&in_flight, "in-flight-effect", RuntimeAttribution::none()),
            executor(),
        )
        .await
        .expect("in-flight operation journals");
    let in_flight_key = in_flight
        .journal_identity()
        .expect("runtime-operation journal identity")
        .key()
        .to_string();

    let core = core_with_host_and_store(Arc::clone(&host), Some(Arc::clone(&store_factory)));
    let session = core
        .session(format!("{gate}-session"))
        .plugin::<JournalPlugin>(JournalConfig {
            host: Arc::clone(&host),
            minted: Minted::default(),
        })
        .open()
        .await
        .expect("session");
    let receipt = session
        .plugin_operations()
        .run_task::<DrainingTaskOp>(json!({ "gate": gate }))
        .await
        .expect("the task returns while its loser drains");
    let scope_key = receipt.output["scope"]
        .as_str()
        .expect("the receipt names its scope")
        .to_string();
    assert_eq!(
        journal.effects(&scope_key).await,
        2,
        "the receipt returned with the loser still draining; its journal survives"
    );
    assert_eq!(journal.in_progress(&scope_key).await, 1);
    assert_eq!(
        journal.fences(&scope_key).await,
        0,
        "a live scope is not fenced"
    );

    let sweep = || async {
        store_factory
            .reclaim_retained_evidence(lash::persistence::RetentionBound {
                committed_before_epoch_ms: 0,
            })
            .await
            .expect("the sweep commits")
    };
    let report = sweep().await;
    assert_eq!(
        report.retired_effect_scope_count, 0,
        "the sweep refuses a scope whose loser is still live: {report:?}"
    );
    assert_eq!(journal.effects(&scope_key).await, 2);
    assert_eq!(journal.fences(&scope_key).await, 0);

    // The facade and the session go away before the drain settles: whatever
    // retires the scope later cannot live in either of them.
    drop(session);
    drop(core);
    drain_gate(&gate).release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while journal.in_progress(&scope_key).await != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the drain settles the loser");
    assert_eq!(
        journal.effects(&scope_key).await,
        2,
        "settling the drain retires nothing by itself"
    );

    let report = sweep().await;
    assert_eq!(
        report.retired_effect_scope_count, 1,
        "the sweep retires the now-quiescent scope with a recorded receipt: {report:?}"
    );
    assert_eq!(
        journal.effects(&scope_key).await,
        0,
        "the scope's rows are gone"
    );
    assert_eq!(journal.fences(&scope_key).await, 1, "the scope is fenced");
    assert_eq!(
        journal.effects(&in_flight_key).await,
        1,
        "a scope without a recorded receipt is never swept"
    );
    assert_eq!(journal.fences(&in_flight_key).await, 0);
    assert_eq!(
        sweep().await.retired_effect_scope_count,
        0,
        "a second sweep finds nothing left to retire"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_draining_task_is_retired_by_the_reclaim_sweep() {
    draining_task_is_retired_by_the_reclaim_sweep(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_draining_task_is_retired_by_the_reclaim_sweep() {
    draining_task_is_retired_by_the_reclaim_sweep(true).await;
}

/// The sweep retires facade-minted scopes only. A caller-supplied
/// runtime-operation id with a recorded receipt is the caller's replay proof
/// for as long as the caller may retry it, so the sweep leaves it alone and
/// an identical retry after the sweep still replays the receipt; a
/// facade-minted scope in the same state is retired (FIG-2499 fix round 3,
/// ruling 3; ADR 0067).
async fn caller_supplied_scope_survives_the_reclaim_sweep(pg: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let label = if pg { "postgres" } else { "sqlite" };
    let mut postgres = None;
    let mut catalog = None;
    let (host, journal, store_factory): (
        Arc<dyn EffectHost>,
        Journal,
        Arc<dyn lash::persistence::SessionStoreFactory>,
    ) = if pg {
        let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
            assert!(
                std::env::var("LASH_REQUIRE_POSTGRES").is_err(),
                "LASH_REQUIRE_POSTGRES=1 but LASH_POSTGRES_DATABASE_URL is not set"
            );
            eprintln!(
                "skipping Postgres caller-scope sweep test: LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        };
        let admin = sqlx::PgPool::connect(&url).await.expect("connect postgres");
        let name = format!("sweep_caller_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create a private database");
        admin.close().await;
        let (base, _) = url.rsplit_once('/').expect("database url has a path");
        let storage = lash_postgres_store::PostgresStorage::connect(&format!("{base}/{name}"))
            .await
            .expect("connect the private database");
        let host = storage.effect_host();
        let factory = storage.session_store_factory_with_shared_process_registry();
        let pool = storage.pool().clone();
        postgres = Some(storage);
        (Arc::new(host), Journal::Postgres(pool), Arc::new(factory))
    } else {
        let path = dir.path().join("caller-sweep.db");
        let host = SqliteEffectHost::open(&path)
            .await
            .expect("SQLite effect host");
        let factory =
            lash_sqlite_store::SqliteSessionStoreFactory::new(dir.path().join("sessions"));
        catalog = Some(factory.catalog_path());
        (Arc::new(host), Journal::Sqlite(path), Arc::new(factory))
    };
    let _postgres = postgres.take();
    store_factory.bind_effect_host(&host);
    let core = core_with_host_and_store(Arc::clone(&host), Some(Arc::clone(&store_factory)));
    let session_id = SessionId::from(format!("caller-sweep-{label}"));
    // The catalog the sweep reads receipts from exists once a session does.
    let session = core
        .session(session_id.clone())
        .open()
        .await
        .expect("session");

    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let run = |scope: ExecutionScope| {
        let host = Arc::clone(&host);
        let ran = Arc::clone(&ran);
        let session_id = session_id.clone();
        async move {
            host.scoped(scope.clone())
                .expect("the operation scope binds")
                .controller()
                .execute_effect(
                    envelope(
                        &scope,
                        "receipted-effect",
                        RuntimeAttribution::for_session(session_id.clone()),
                    ),
                    RuntimeEffectLocalExecutor::testing(move |_| {
                        let ran = Arc::clone(&ran);
                        async move {
                            ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                                value: json!({ "ran": true }),
                            })
                        }
                    }),
                )
                .await
                .map(|_| ())
                .map_err(|error| error.code)
        }
    };
    let executions = || ran.load(std::sync::atomic::Ordering::SeqCst);
    // Record the receipt fact the sweep selects on: the operation's receipt
    // key in the session's commit ledger, as a completed operation leaves it.
    let record_receipt = |scope: &ExecutionScope| {
        let receipt = lash_core::store::plugin_operation_receipt_storage_key(scope)
            .expect("receipt storage key");
        let session_id = session_id.clone();
        let journal = &journal;
        let catalog = catalog.clone();
        async move {
            match journal {
                Journal::Sqlite(_) => {
                    rusqlite::Connection::open(catalog.expect("sqlite catalog"))
                        .expect("open the catalog")
                        .execute(
                            "INSERT INTO runtime_turn_commits (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms) VALUES (?1, ?2, 'witness', '{}', 0)",
                            rusqlite::params![session_id.as_str(), receipt],
                        )
                        .expect("record the receipt");
                }
                Journal::Postgres(pool) => {
                    sqlx::query(
                        "INSERT INTO lash_runtime_turn_commits (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms) VALUES ($1, $2, 'witness', '{}', 0)",
                    )
                    .bind(session_id.as_str())
                    .bind(receipt)
                    .execute(pool)
                    .await
                    .expect("record the receipt");
                }
            }
        }
    };

    let caller = ExecutionScope::runtime_operation("caller-supplied-stable-request");
    let minted = ExecutionScope::runtime_operation(lash_core::store::mint_facade_operation_id(
        &session_id,
        lash_core::store::FacadePluginOperation::Task,
        "sweep_task",
    ));
    let caller_key = caller
        .journal_identity()
        .expect("journal identity")
        .key()
        .to_string();
    let minted_key = minted
        .journal_identity()
        .expect("journal identity")
        .key()
        .to_string();
    for scope in [&caller, &minted] {
        run(scope.clone()).await.expect("the operation journals");
        record_receipt(scope).await;
    }
    assert_eq!(executions(), 2);
    run(caller.clone())
        .await
        .expect("an identical retry replays before the sweep");
    assert_eq!(executions(), 2, "the retry replayed the journal");

    let report = store_factory
        .reclaim_retained_evidence(lash::persistence::RetentionBound {
            committed_before_epoch_ms: 0,
        })
        .await
        .expect("the sweep commits");
    assert_eq!(
        report.retired_effect_scope_count, 1,
        "the sweep retires the facade-minted scope and only it: {report:?}"
    );
    assert_eq!(
        journal.effects(&caller_key).await,
        1,
        "the caller's journal survives"
    );
    assert_eq!(
        journal.fences(&caller_key).await,
        0,
        "the caller's scope is not fenced"
    );
    assert_eq!(
        journal.effects(&minted_key).await,
        0,
        "the minted scope's rows are gone"
    );
    assert_eq!(
        journal.fences(&minted_key).await,
        1,
        "the minted scope is fenced"
    );

    run(caller.clone())
        .await
        .expect("the caller's identical retry replays its receipt after the sweep");
    assert_eq!(executions(), 2, "the post-sweep retry re-executed nothing");
    assert_eq!(
        run(minted.clone()).await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired),
        "the facade-minted scope is retired"
    );
    drop(session);
    drop(core);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_caller_supplied_scope_survives_the_reclaim_sweep() {
    caller_supplied_scope_survives_the_reclaim_sweep(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_caller_supplied_scope_survives_the_reclaim_sweep() {
    caller_supplied_scope_survives_the_reclaim_sweep(true).await;
}

/// The maintenance sweep is another public retirement path, so it must hold
/// the same promise-owner participant fence as direct host retirement. A
/// receipted, quiescent operation stays replayable while its closure is pinned
/// and retires normally after the catalog consumes and releases that pin.
async fn reclaim_sweep_respects_turn_cancel_closure_participant(pg: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let label = if pg { "postgres" } else { "sqlite" };
    let mut postgres = None;
    let mut sqlite_catalog = None;
    let (host, journal, factory): (
        Arc<dyn EffectHost>,
        Journal,
        Arc<dyn lash::persistence::SessionStoreFactory>,
    ) = if pg {
        let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
            assert!(
                std::env::var("LASH_REQUIRE_POSTGRES").is_err(),
                "LASH_REQUIRE_POSTGRES=1 but LASH_POSTGRES_DATABASE_URL is not set"
            );
            eprintln!("skipping Postgres pinned-sweep test: database URL is not set");
            return;
        };
        let admin = sqlx::PgPool::connect(&url).await.expect("connect postgres");
        let name = format!("pinned_sweep_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create a private database");
        admin.close().await;
        let (base, _) = url.rsplit_once('/').expect("database url has a path");
        let storage = lash_postgres_store::PostgresStorage::connect(&format!("{base}/{name}"))
            .await
            .expect("connect the private database");
        let host = storage.effect_host();
        let factory = storage.session_store_factory_with_shared_process_registry();
        let pool = storage.pool().clone();
        postgres = Some(storage);
        (Arc::new(host), Journal::Postgres(pool), Arc::new(factory))
    } else {
        let effect_path = dir.path().join("pinned-sweep-effects.db");
        let catalog_root = dir.path().join("pinned-sweep-sessions");
        let host = SqliteEffectHost::open(&effect_path)
            .await
            .expect("SQLite effect host");
        let factory = lash_sqlite_store::SqliteSessionStoreFactory::new(&catalog_root);
        sqlite_catalog = Some(factory.catalog_path());
        (
            Arc::new(host),
            Journal::Sqlite(effect_path),
            Arc::new(factory),
        )
    };
    let _postgres = postgres.take();
    factory.bind_effect_host(&host);

    let session_id = SessionId::from(format!("pinned-sweep-{label}"));
    let address = lash_core::runtime::TurnAddress::new(&session_id, "turn");
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create pinned-sweep session");
    let lease = store
        .try_claim_session_execution_lease(
            &session_id,
            &lash_core::LeaseOwnerIdentity::opaque(label, format!("{label}:incarnation")),
            &format!("{label}:executor"),
            60_000,
        )
        .await
        .expect("claim pinned-sweep lease")
        .acquired()
        .expect("pinned-sweep lease is free");
    let scope = ExecutionScope::runtime_operation(lash_core::store::mint_facade_operation_id(
        &session_id,
        lash_core::store::FacadePluginOperation::Task,
        "pinned_sweep",
    ));
    host.scoped(scope.clone())
        .expect("scope operation")
        .controller()
        .execute_effect(
            envelope(&scope, "pinned-sweep-effect", RuntimeAttribution::none()),
            executor(),
        )
        .await
        .expect("journal quiescent operation effect");
    let scope_key = scope
        .journal_identity()
        .expect("operation journal identity")
        .key()
        .to_string();
    let receipt_key = lash_core::store::plugin_operation_receipt_storage_key(&scope)
        .expect("operation receipt key");
    match &journal {
        Journal::Sqlite(_) => {
            rusqlite::Connection::open(sqlite_catalog.expect("sqlite catalog"))
                .expect("open session catalog")
                .execute(
                    "INSERT INTO runtime_turn_commits (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms) VALUES (?1, ?2, 'witness', '{}', 0)",
                    rusqlite::params![session_id.as_str(), receipt_key],
                )
                .expect("record operation receipt");
        }
        Journal::Postgres(pool) => {
            sqlx::query(
                "INSERT INTO lash_runtime_turn_commits (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms) VALUES ($1, $2, 'witness', '{}', 0)",
            )
            .bind(session_id.as_str())
            .bind(receipt_key)
            .execute(pool)
            .await
            .expect("record operation receipt");
        }
    }

    let scoped = host.scoped(scope.clone()).expect("scope closure owner");
    let binding = host
        .turn_control_binding(&scoped)
        .await
        .expect("bind closure owner");
    store
        .validate_turn_cancellation_binding(
            &session_id,
            &lease.fence(),
            binding.binding_id(),
            &scope,
        )
        .await
        .expect("persist operation admission scope");
    let resolver = binding.resolver();
    let authorization = lash_core::TurnCancelClosureAuthorization::new(
        address.clone(),
        binding.binding_id(),
        scope.clone(),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnCancelGate,
            )
            .await
            .expect("base key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnCancelEscalation,
            )
            .await
            .expect("escalation key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnTerminal,
            )
            .await
            .expect("terminal key"),
        lash_core::TurnCancelClosureProposal::CompletionSealed,
        lash_core::TurnCancelIntentSnapshot::Absent,
        &lease.fence(),
    )
    .expect("materialize operation closure");
    store
        .authorize_turn_cancel_closure(&lease.fence(), &authorization)
        .await
        .expect("authorize and register operation closure");

    let sweep = || async {
        factory
            .reclaim_retained_evidence(lash::persistence::RetentionBound {
                committed_before_epoch_ms: 0,
            })
            .await
            .expect("reclaim sweep commits")
    };
    let blocked = sweep().await;
    assert_eq!(
        blocked.retired_effect_scope_count, 0,
        "a closure participant blocks maintenance retirement: {blocked:?}"
    );
    assert_eq!(journal.effects(&scope_key).await, 1);
    assert_eq!(journal.fences(&scope_key).await, 0);
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read closure pins")
            .len(),
        1
    );

    let authority =
        lash_core::TurnCancellationAuthority::new(host.turn_control_binding_id(), host.clone());
    let settlement = authority
        .settle_authorized_closure(&authorization)
        .await
        .expect("settle operation closure");
    store
        .repair_orphaned_active_turn_inputs(
            &session_id,
            &lease.fence(),
            &address.turn_id,
            authorization.observed_intent(),
            Some(&settlement),
        )
        .await
        .expect("consume operation closure")
        .into_applied()
        .expect("operation repair applies");
    factory
        .retire_turn_cancel_closure_scope(&scope)
        .await
        .expect("retire catalog scope and release participant");

    let retired = sweep().await;
    assert_eq!(retired.retired_effect_scope_count, 1, "{retired:?}");
    assert_eq!(journal.effects(&scope_key).await, 0);
    assert_eq!(journal.fences(&scope_key).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_reclaim_sweep_respects_turn_cancel_closure_participant() {
    reclaim_sweep_respects_turn_cancel_closure_participant(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_reclaim_sweep_respects_turn_cancel_closure_participant() {
    reclaim_sweep_respects_turn_cancel_closure_participant(true).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParticipantCrashBoundary {
    AfterOwnerRegister,
    BeforeOwnerRelease,
}

struct ParticipantCrashHost {
    inner: Arc<dyn EffectHost>,
    boundary: ParticipantCrashBoundary,
    marker: std::path::PathBuf,
}

impl ParticipantCrashHost {
    async fn stop_at_boundary(&self) -> ! {
        std::fs::write(&self.marker, b"durable boundary reached\n")
            .expect("write participant crash marker");
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for ParticipantCrashHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }
}

#[async_trait::async_trait]
impl EffectHost for ParticipantCrashHost {
    fn turn_control_binding_id(&self) -> String {
        self.inner.turn_control_binding_id()
    }

    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.inner.scoped(scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.inner.scoped_static(scope)
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self.inner.await_event_resolver()
    }

    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .register_turn_cancel_closure_participant(participant_id, scope)
            .await?;
        if self.boundary == ParticipantCrashBoundary::AfterOwnerRegister {
            self.stop_at_boundary().await;
        }
        Ok(())
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        if self.boundary == ParticipantCrashBoundary::BeforeOwnerRelease {
            self.stop_at_boundary().await;
        }
        self.inner
            .release_turn_cancel_closure_participant(participant_id, scope)
            .await
    }
}

async fn participant_crash_handles(
    backend: &str,
    locator: &str,
) -> (Arc<dyn EffectHost>, Arc<dyn lash_core::SessionStoreFactory>) {
    if backend == "postgres" {
        let storage = lash_postgres_store::PostgresStorage::connect(locator)
            .await
            .expect("connect participant-crash PostgreSQL database");
        (
            Arc::new(storage.effect_host()),
            Arc::new(storage.session_store_factory_with_shared_process_registry()),
        )
    } else {
        let root = std::path::Path::new(locator);
        let host = SqliteEffectHost::open(&root.join("effects.db"))
            .await
            .expect("open participant-crash SQLite owner");
        (
            Arc::new(host),
            Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                root.join("catalog"),
            )),
        )
    }
}

async fn authorize_participant_crash_closure(
    host: &Arc<dyn EffectHost>,
    factory: &Arc<dyn lash_core::SessionStoreFactory>,
    scenario: &str,
    scope: &ExecutionScope,
) -> (
    Arc<dyn lash_core::RuntimePersistence>,
    lash_core::SessionExecutionLease,
    lash_core::TurnCancelClosureAuthorization,
) {
    factory.bind_effect_host(host);
    let session_id = SessionId::from(format!("participant-crash-{scenario}"));
    let address = lash_core::runtime::TurnAddress::new(&session_id, "turn");
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create participant-crash session");
    let lease = store
        .try_claim_session_execution_lease(
            &session_id,
            &lash_core::LeaseOwnerIdentity::opaque(scenario, format!("{scenario}:incarnation")),
            &format!("{scenario}:executor"),
            60_000,
        )
        .await
        .expect("claim participant-crash lease")
        .acquired()
        .expect("participant-crash lease is free");
    let scoped = host.scoped(scope.clone()).expect("scope participant owner");
    let binding = host
        .turn_control_binding(&scoped)
        .await
        .expect("bind participant owner");
    store
        .validate_turn_cancellation_binding(
            &session_id,
            &lease.fence(),
            binding.binding_id(),
            scope,
        )
        .await
        .expect("persist participant-crash admission");
    let resolver = binding.resolver();
    let authorization = lash_core::TurnCancelClosureAuthorization::new(
        address.clone(),
        binding.binding_id(),
        scope.clone(),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnCancelGate,
            )
            .await
            .expect("participant base key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnCancelEscalation,
            )
            .await
            .expect("participant escalation key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnTerminal,
            )
            .await
            .expect("participant terminal key"),
        lash_core::TurnCancelClosureProposal::CompletionSealed,
        lash_core::TurnCancelIntentSnapshot::Absent,
        &lease.fence(),
    )
    .expect("materialize participant-crash closure");
    (store, lease, authorization)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawned and killed by the participant lifecycle crash laws"]
async fn participant_protocol_crash_child() {
    let backend = std::env::var("LASH_PARTICIPANT_CRASH_BACKEND").expect("child backend");
    let locator = std::env::var("LASH_PARTICIPANT_CRASH_LOCATOR").expect("child locator");
    let scenario = std::env::var("LASH_PARTICIPANT_CRASH_SCENARIO").expect("child scenario");
    let marker = std::env::var_os("LASH_PARTICIPANT_CRASH_MARKER")
        .map(std::path::PathBuf::from)
        .expect("child marker");
    let boundary = match std::env::var("LASH_PARTICIPANT_CRASH_BOUNDARY")
        .expect("child boundary")
        .as_str()
    {
        "register" => ParticipantCrashBoundary::AfterOwnerRegister,
        "release" => ParticipantCrashBoundary::BeforeOwnerRelease,
        boundary => panic!("unknown participant crash boundary {boundary}"),
    };
    let (inner, factory) = participant_crash_handles(&backend, &locator).await;
    let host: Arc<dyn EffectHost> = Arc::new(ParticipantCrashHost {
        inner,
        boundary,
        marker,
    });
    factory.bind_effect_host(&host);
    let scope = ExecutionScope::runtime_operation(format!("participant-crash-{scenario}"));
    if boundary == ParticipantCrashBoundary::AfterOwnerRegister {
        let (store, lease, authorization) =
            authorize_participant_crash_closure(&host, &factory, &scenario, &scope).await;
        store
            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
            .await
            .expect("register boundary never returns before the parent kills this child");
    } else {
        factory
            .retire_turn_cancel_closure_scope(&scope)
            .await
            .expect("release boundary never returns before the parent kills this child");
    }
    panic!("participant crash child passed its deterministic kill boundary");
}

fn kill_child_at_participant_boundary(
    backend: &str,
    locator: &str,
    scenario: &str,
    boundary: &str,
    marker: &std::path::Path,
) {
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("locate participant-crash test binary"),
    )
    .args([
        "--exact",
        "participant_protocol_crash_child",
        "--ignored",
        "--nocapture",
    ])
    .env("LASH_PARTICIPANT_CRASH_BACKEND", backend)
    .env("LASH_PARTICIPANT_CRASH_LOCATOR", locator)
    .env("LASH_PARTICIPANT_CRASH_SCENARIO", scenario)
    .env("LASH_PARTICIPANT_CRASH_BOUNDARY", boundary)
    .env("LASH_PARTICIPANT_CRASH_MARKER", marker)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn participant-crash child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !marker.exists() {
        if let Some(status) = child.try_wait().expect("poll participant-crash child") {
            panic!("participant-crash child exited before its boundary: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "participant-crash child did not reach {boundary} boundary"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child
        .kill()
        .expect("kill child at durable participant boundary");
    let status = child.wait().expect("reap participant-crash child");
    assert!(!status.success(), "the boundary child must be killed");
}

async fn private_participant_crash_postgres_url(label: &str) -> Option<String> {
    let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
        assert!(
            std::env::var("LASH_REQUIRE_POSTGRES").is_err(),
            "LASH_REQUIRE_POSTGRES=1 but LASH_POSTGRES_DATABASE_URL is not set"
        );
        eprintln!("skipping PostgreSQL participant-crash law: database URL is not set");
        return None;
    };
    let admin = sqlx::PgPool::connect(&url)
        .await
        .expect("connect participant-crash PostgreSQL admin");
    let name = format!(
        "participant_crash_{label}_{}",
        uuid::Uuid::new_v4().simple()
    );
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .expect("create participant-crash database");
    admin.close().await;
    let (base, _) = url.rsplit_once('/').expect("database URL has a path");
    Some(format!("{base}/{name}"))
}

async fn participant_protocol_survives_both_crash_windows(backend: &str, locator: &str) {
    let evidence = tempfile::tempdir().expect("participant crash marker directory");

    let register_scenario = format!("{backend}-register");
    let register_scope =
        ExecutionScope::runtime_operation(format!("participant-crash-{register_scenario}"));
    let register_marker = evidence.path().join("after-owner-register");
    kill_child_at_participant_boundary(
        backend,
        locator,
        &register_scenario,
        "register",
        &register_marker,
    );
    let (register_host, register_factory) = participant_crash_handles(backend, locator).await;
    register_factory.bind_effect_host(&register_host);
    let register_session = SessionId::from(format!("participant-crash-{register_scenario}"));
    assert_eq!(
        register_factory
            .pending_turn_cancel_closure_pins(&register_session)
            .await
            .expect("inspect register-crash local pins")
            .len(),
        0,
        "the crash happened before the local authorization committed"
    );
    assert!(
        register_host
            .retire_effect_journal(
                EffectJournalRetirement::for_scope(&register_scope).expect("retirable scope")
            )
            .await
            .is_err(),
        "the committed owner participant survives the register crash"
    );
    register_factory
        .retire_turn_cancel_closure_scope(&register_scope)
        .await
        .expect("restart fences the empty catalog scope and releases the orphan participant");
    register_factory
        .retire_turn_cancel_closure_scope(&register_scope)
        .await
        .expect("orphan participant release is idempotent");
    register_host
        .retire_effect_journal(
            EffectJournalRetirement::for_scope(&register_scope).expect("retirable scope"),
        )
        .await
        .expect("owner scope retires after orphan participant recovery");

    let release_scenario = format!("{backend}-release");
    let release_scope =
        ExecutionScope::runtime_operation(format!("participant-crash-{release_scenario}"));
    let (release_host, release_factory) = participant_crash_handles(backend, locator).await;
    let (store, lease, authorization) = authorize_participant_crash_closure(
        &release_host,
        &release_factory,
        &release_scenario,
        &release_scope,
    )
    .await;
    store
        .authorize_turn_cancel_closure(&lease.fence(), &authorization)
        .await
        .expect("authorize release-crash closure");
    let authority = lash_core::TurnCancellationAuthority::new(
        release_host.turn_control_binding_id(),
        release_host.clone(),
    );
    let settlement = authority
        .settle_authorized_closure(&authorization)
        .await
        .expect("settle release-crash closure");
    store
        .repair_orphaned_active_turn_inputs(
            authorization.session_id(),
            &lease.fence(),
            authorization.turn_id(),
            authorization.observed_intent(),
            Some(&settlement),
        )
        .await
        .expect("consume release-crash authorization")
        .into_applied()
        .expect("release-crash repair applies");
    drop(store);
    drop(release_factory);
    drop(release_host);

    let release_marker = evidence.path().join("before-owner-release");
    kill_child_at_participant_boundary(
        backend,
        locator,
        &release_scenario,
        "release",
        &release_marker,
    );
    let (release_host, release_factory) = participant_crash_handles(backend, locator).await;
    release_factory.bind_effect_host(&release_host);
    assert!(
        release_host
            .retire_effect_journal(
                EffectJournalRetirement::for_scope(&release_scope).expect("retirable scope")
            )
            .await
            .is_err(),
        "the owner participant remains after local retirement crashes before release"
    );
    release_factory
        .retire_turn_cancel_closure_scope(&release_scope)
        .await
        .expect("restart repeats the local fence and releases the retained participant");
    release_factory
        .retire_turn_cancel_closure_scope(&release_scope)
        .await
        .expect("post-crash participant release is idempotent");
    release_host
        .retire_effect_journal(
            EffectJournalRetirement::for_scope(&release_scope).expect("retirable scope"),
        )
        .await
        .expect("owner retirement succeeds after release recovery");
    println!(
        "participant crash cuts passed: backend={backend} after_owner_register=1 before_owner_release=1"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_participant_protocol_survives_register_and_release_process_crashes() {
    let root = tempfile::tempdir().expect("SQLite participant-crash root");
    participant_protocol_survives_both_crash_windows(
        "sqlite",
        root.path().to_str().expect("UTF-8 SQLite crash path"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_participant_protocol_survives_register_and_release_process_crashes() {
    let Some(url) = private_participant_crash_postgres_url("both_boundaries").await else {
        return;
    };
    participant_protocol_survives_both_crash_windows("postgres", &url).await;
}
