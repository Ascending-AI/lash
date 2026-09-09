//! The facade retires the runtime-operation scope it mints for every plugin
//! task once the receipt — or the failure — is back, through the same
//! `EffectJournalRetirement` lever hosts reach for process and session
//! journals (FIG-2499, FIG-2500). A runtime operation the facade did not mint
//! is left alone.

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
    type Input = ();

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
            .typed_task_value::<DrainingTaskOp, _, _>(|ctx, _args| async move {
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
                drain_gate().started.notified().await;
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

fn drain_gate() -> &'static DrainGate {
    static GATE: std::sync::OnceLock<DrainGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| DrainGate {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    })
}

/// Group executors for the SQLite host: the draining child waits on its gate,
/// everything else settles at once.
struct DrainExecutors;

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
            RuntimeEffectLocalExecutor::testing(|_| async {
                drain_gate().started.notify_one();
                drain_gate().release.notified().await;
                Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                    value: json!({ "drained": true }),
                })
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
    LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(provider)
        .model(
            lash::ModelSpec::builder("mock-model")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
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
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
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

/// A task that returns while a run-to-completion loser is still draining
/// keeps its journal: the receipt is not proof that the scope is quiescent,
/// so retirement is deferred, and the next plugin operation on the session
/// retires the scope once the drain has settled (FIG-2499 review round 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_task_with_a_draining_group_keeps_its_journal_until_quiescent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drain-retirement.db");
    let sqlite = SqliteEffectHost::open(&path)
        .await
        .expect("SQLite effect host");
    sqlite
        .register_group_executors(Arc::new(DrainExecutors))
        .expect("register group executors");
    let host: Arc<dyn EffectHost> = Arc::new(sqlite);
    let core = core_with_host(Arc::clone(&host));
    let session = core
        .session("drain-retirement")
        .plugin::<JournalPlugin>(JournalConfig {
            host: Arc::clone(&host),
            minted: Minted::default(),
        })
        .open()
        .await
        .expect("session");
    let operations = session.plugin_operations();
    let receipt = operations
        .run_task::<DrainingTaskOp>(json!({}))
        .await
        .expect("the task returns while its loser drains");
    let scope_key = receipt.output["scope"]
        .as_str()
        .expect("the receipt names its scope")
        .to_string();

    let conn = rusqlite::Connection::open(&path).expect("open the effect journal");
    let count = |sql: &str| -> i64 {
        conn.query_row(sql, [&scope_key], |row| row.get(0))
            .expect("count rows")
    };
    let effects = "SELECT COUNT(*) FROM runtime_effect_replay WHERE scope_id = ?1";
    let in_progress =
        "SELECT COUNT(*) FROM runtime_effect_replay WHERE scope_id = ?1 AND status = 'in_progress'";
    let fences = "SELECT COUNT(*) FROM effect_scope_retirements WHERE scope_id = ?1";
    assert_eq!(
        count(effects),
        2,
        "the receipt returned with the loser still draining; its journal survives"
    );
    assert_eq!(
        count(in_progress),
        1,
        "the loser is journaled as in progress"
    );
    assert_eq!(count(fences), 0, "a live scope is not fenced");

    drain_gate().release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while count(in_progress) != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the drain settles the loser");
    assert_eq!(
        count(effects),
        2,
        "settling the drain retires nothing by itself"
    );

    // The next plugin operation on the session retries the deferred
    // retirement first; the scope is quiescent now, so it goes.
    operations
        .run_command::<JournalCommandOp>(json!({}))
        .await
        .expect("a later operation runs");
    assert_eq!(count(effects), 0, "the quiescent scope's rows are retired");
    assert_eq!(count(fences), 1, "the quiescent scope is fenced");
}
