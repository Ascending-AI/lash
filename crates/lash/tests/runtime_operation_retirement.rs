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
use lash_core::facade_support::ScopedEffectControllerFacadeOps;
use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeInvocation, RuntimeScope,
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
                let controller = ctx.scoped_effect_controller.controller();
                let group = lash_core::RuntimeEffectGroup::try_new(
                    RuntimeInvocation::effect(
                        RuntimeScope::for_turn("op-retirement-session", "op-retirement-turn", 1, 0),
                        "drain-group",
                        RuntimeEffectKind::LanguageRuntimeValue,
                        format!("{scope_key}:drain-group"),
                    ),
                    format!("{scope_key}:drain-group"),
                    vec![envelope("fast-child"), envelope("draining-child")],
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
        .execute_effect(envelope("task-effect"), executor())
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

fn envelope(effect_id: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("op-retirement-session", "op-retirement-turn", 1, 0),
            effect_id,
            RuntimeEffectKind::LanguageRuntimeValue,
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
        .execute_effect(envelope("in-flight-effect"), executor())
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
        .execute_effect(envelope("in-flight-effect"), executor())
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
        async move {
            host.scoped(scope)
                .expect("the operation scope binds")
                .controller()
                .execute_effect(
                    envelope("receipted-effect"),
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
