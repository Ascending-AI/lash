//! The facade retires the runtime-operation scope it mints for every plugin
//! task once the receipt — or the failure — is back, through the same
//! `EffectJournalRetirement` lever hosts reach for process and session
//! journals (FIG-2499, FIG-2500). A runtime operation the facade did not mint
//! is left alone.

use std::sync::{Arc, Mutex};

use lash::durability::{EffectHost, EffectJournalRetirement};
use lash::plugins::{
    PluginError, PluginFactory, PluginOperation, PluginOperationFailure, PluginRegistrar,
    PluginSessionContext, PluginTask, PluginTaskContext, SessionParam, SessionPlugin,
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
