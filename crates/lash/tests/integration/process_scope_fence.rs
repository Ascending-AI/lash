//! The `process:{id}` scope fence around a process prune: only rows the
//! registry actually prunes are fenced (watermark and pending-work guards
//! are honoured), and a pruned id stays fenced for good (FIG-2499, ADR 0049).
//! An id is minted and never reused (ADR 0107), so starting the same work
//! again after a prune is a new process under a new, unfenced scope, and a
//! redrive of a recorded start answers the pruned process it recorded.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use std::sync::Arc;

use lash::LashCore;
use lash::durability::EffectHost;
use lash::persistence::SessionStoreFactory as _;
use lash_core::{
    AwaitEventWaitIdentity, EffectAddress, ExecutionScope, Resolution, RuntimeAttribution,
    RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use serde_json::json;

fn envelope(scope: &ExecutionScope, effect_id: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), effect_id)
                .expect("process fence effect carries an admitted scope"),
            RuntimeAttribution::none(),
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
            value: json!({ "ran": true }),
        })
    })
}

/// The facade core under test, over `backend`.
fn core_over(backend: lash::Backend) -> LashCore {
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
    LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(provider)
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .model(
            lash::ModelSpec::builder("mock-model")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "process-fence-test-worker",
            "process-fence-test-boot",
        ))
        .expect("core")
}

/// Counts the fence rows a durable backend holds for one scope key.
type FenceCounter = Box<
    dyn Fn(&str) -> std::pin::Pin<Box<dyn std::future::Future<Output = i64> + Send>> + Send + Sync,
>;

/// Opens a fresh host over the same durable journal.
type ColdHost = Box<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Arc<dyn EffectHost>> + Send>>
        + Send
        + Sync,
>;

/// One substrate under test: its backend, effect host and process registry,
/// plus the row observations the substrate exposes.
struct Backend {
    backend: lash::Backend,
    host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    fences: Option<FenceCounter>,
    /// Where a fresh host over the same durable journal comes from: a cold
    /// observer proves persisted state rather than a warm fence cache.
    cold_host: Option<ColdHost>,
    /// The SQLite backend, for its catalog's retention sweep.
    sqlite: Option<Arc<lash_sqlite_store::SqliteBackend>>,
    /// The SQLite registry database and effect journal, for the two-file
    /// layout witnesses.
    sqlite_files: Option<(std::path::PathBuf, std::path::PathBuf)>,
    _dir: tempfile::TempDir,
}

enum Kind {
    Sqlite,
}

/// A SQLite substrate: fences counted in both its registry and
/// its journal, and a cold host from a reopen of the same location.
fn sqlite_backend(
    backend: lash_sqlite_store::SqliteBackend,
    dir: tempfile::TempDir,
    files: Option<(std::path::PathBuf, std::path::PathBuf)>,
) -> Backend {
    let backend = Arc::new(backend);
    let fence_journal = backend.database_uri(lash_sqlite_store::SqliteDatabase::EffectReplay);
    let fence_registry = backend.database_uri(lash_sqlite_store::SqliteDatabase::ProcessRegistry);
    let cold_backend = Arc::clone(&backend);
    Backend {
        host: backend.effect_host(),
        registry: backend.process_registry(),
        backend: lash::Backend::new(backend.clone()),
        cold_host: Some(Box::new(move || {
            let backend = Arc::clone(&cold_backend);
            Box::pin(async move {
                // A fresh host over the same location: it reaches the
                // registry's fences through the backend, not a binding.
                backend
                    .reopen()
                    .await
                    .expect("reopen the backend")
                    .effect_host() as Arc<dyn EffectHost>
            })
        })),
        sqlite: Some(backend),
        sqlite_files: files,
        fences: Some(Box::new(move |scope_id: &str| {
            let journal = fence_journal.clone();
            let registry = fence_registry.clone();
            let scope_id = scope_id.to_string();
            Box::pin(async move {
                // The fence is one row in one of the two databases: count
                // both so the witness reads the layout, not an assumption
                // about which one holds it.
                sqlite_fence_rows(&journal, &scope_id) + sqlite_fence_rows(&registry, &scope_id)
            })
        })),
        _dir: dir,
    }
}

async fn backend(kind: Kind) -> Option<Backend> {
    let dir = tempfile::tempdir().expect("tempdir");
    Some(match kind {
        Kind::Sqlite => {
            let backend = lash_sqlite_store::SqliteBackend::open(dir.path())
                .await
                .expect("SQLite file backend");
            let files = (
                dir.path()
                    .join(lash_sqlite_store::SqliteDatabase::ProcessRegistry.file_name()),
                dir.path()
                    .join(lash_sqlite_store::SqliteDatabase::EffectReplay.file_name()),
            );
            sqlite_backend(backend, dir, Some(files))
        }
    })
}

impl Backend {
    async fn fence_count(&self, scope: &ExecutionScope) -> Option<i64> {
        let key = scope
            .journal_identity()
            .expect("process journal identity")
            .key()
            .to_string();
        match &self.fences {
            Some(fences) => Some(fences(&key).await),
            None => None,
        }
    }
}

/// An external process, keyed by `start_key` when the test starts the same
/// work again.
fn external_registration(start_key: Option<&str>) -> lash_core::ProcessRegistration {
    lash_core::ProcessRegistration::new(
        lash_core::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_start_key(
        start_key.map(|key| lash_core::StartKey::for_host(lash_core::StartKeyOwner::HOST, key)),
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        lash_core::ProcessIdentity::new("test"),
    ))
}

/// Registers and completes one process, answering the id the registrar
/// minted.
async fn register_and_complete(
    registry: &dyn lash_core::ProcessRegistry,
    start_key: Option<&str>,
) -> ProcessId {
    let process_id = registry
        .register_process(external_registration(start_key))
        .await
        .expect("register the process")
        .id;
    registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                json!("done"),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete the process");
    process_id
}

async fn admission(
    host: &dyn EffectHost,
    admitted: &lash_core::AdmittedScope,
    effect_id: &str,
) -> Result<(), lash_core::RuntimeErrorCode> {
    host.scoped(admitted.clone())
        .expect("scope binds")
        .controller()
        .execute_effect(envelope(admitted.scope(), effect_id), executor())
        .await
        .map(|_| ())
        .map_err(|err| err.code)
}

/// A process the registry keeps — here because the caller's projection
/// watermark has not reached its change — keeps its journal and its resolved
/// promise: the facade fences exactly what the registry prunes (FIG-2499 review round 1).
async fn prune_fences_only_what_the_registry_prunes(kind: Kind) {
    let Some(backend) = backend(kind).await else {
        return;
    };
    let core = core_over(backend.backend.clone());
    let process_id = register_and_complete(backend.registry.as_ref(), None).await;
    let scope = ExecutionScope::process(process_id.clone());
    let key = backend
        .host
        .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("kept"))
        .await
        .expect("mint under the process scope");
    backend
        .host
        .resolve_await_event(&key, Resolution::Ok(json!("kept")))
        .await
        .expect("resolve");

    let report = core
        .processes()
        .prune(
            u64::MAX,
            None,
            lash_core::ProjectionWatermark::UpTo(lash_core::ProcessChangeCursor::initial()),
        )
        .await
        .expect("prune honours the watermark");
    assert_eq!(report.pruned_processes, 0);
    assert!(
        backend
            .registry
            .get_process(&process_id)
            .await
            .expect("read the process")
            .is_some(),
        "the registry keeps the process"
    );
    assert_eq!(
        backend.host.peek_await_event(&key).await.expect("peek"),
        Some(Resolution::Ok(json!("kept"))),
        "an unpruned process keeps its resolved promise"
    );
    assert_eq!(backend.fence_count(&scope).await.unwrap_or(0), 0);
    admission(
        backend.host.as_ref(),
        &lash_core::AdmittedScope::process(process_id.clone()),
        "still-admitted",
    )
    .await
    .expect("an unpruned process is still admitted");

    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune without a projector");
    assert_eq!(report.pruned_processes, 1);
    assert_eq!(
        admission(
            backend.host.as_ref(),
            &lash_core::AdmittedScope::process(process_id.clone()),
            "after-prune",
        )
        .await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired),
        "a pruned process is fenced"
    );
    let err = backend
        .host
        .peek_await_event(&key)
        .await
        .expect_err("a pruned process's promise no longer reads");
    assert_eq!(err.code.as_str(), "await_event_unknown_or_revoked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_prune_fences_only_what_the_registry_prunes() {
    prune_fences_only_what_the_registry_prunes(Kind::Sqlite).await;
}

/// Every registrant that ends in the registry insert.
#[derive(Clone, Copy, Debug)]
enum RegistrationPath {
    /// A host registering directly on the registry.
    DirectRegistry,
    /// `Processes::start` on the core.
    CoreStart,
    /// `processes().start` on an open session.
    SessionStart,
    /// The trigger router delivering an occurrence.
    TriggerRouter,
    /// The tool-intent ingress realizing a `StartProcess` intent.
    ToolIntentIngress,
}

const RESTART_SESSION: &str = "fence-restart-session";
const RESTART_KEY: &str = "fence-restart";

fn start_request() -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessOriginator::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_start_key(Some(lash_core::StartKey::for_host(
        lash_core::StartKeyOwner::HOST,
        RESTART_KEY,
    )))
}

async fn register_trigger_subscription(
    store: &dyn lash_core::TriggerStore,
    env_store: &dyn lash::persistence::ProcessExecutionEnvStore,
    occurrence: &lash_core::TriggerOccurrenceRequest,
) {
    let process_env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        env_store,
        &lash_core::ArtifactOwner::host("process-fence-trigger"),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash::plugins::PluginOptions::default(),
            lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded),
        ),
    )
    .await
    .expect("publish the subscription's execution environment");
    let draft = lash_core::TriggerSubscriptionDraft::for_process(
        "test/fence-restart",
        process_env_ref,
        "ui.button.pressed",
        occurrence.source_key.clone(),
        lash_core::ProcessInput::Engine {
            kind: "testing-fixture".to_string(),
            payload: json!({}),
        },
        lash_core::ProcessIdentity::new("testing-fixture"),
    )
    .with_payload_schema(lash_core::LashSchema::any());
    let outcome = store
        .execute_command(
            "fence-restart-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("fence-test").expect("host scope"),
                actor: lash_core::ProcessOriginator::host_scoped("fence-test"),
                draft,
            },
        )
        .await
        .expect("execute the registration")
        .expect("register the subscription");
    assert!(matches!(
        outcome,
        lash_core::TriggerCommandOutcome::Mutation { .. }
    ));
}

/// Every registrant's start of the same work after its process was pruned
/// (FIG-3611, ADR 0107). The pruned id stays fenced for good. A host start
/// under the same key is a new process with a new id and an unfenced scope; a
/// trigger delivery bound to the pruned process and a redrive of a recorded
/// tool-intent start both answer the pruned process and register nothing.
async fn restarting_pruned_work(kind: Kind, path: RegistrationPath) {
    let Some(backend) = backend(kind).await else {
        return;
    };
    let trigger_store = backend.backend.trigger_store();
    let core = core_over(backend.backend.clone());
    let occurrence = lash_core::TriggerOccurrenceRequest::new(
        "ui.button.pressed",
        lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
            .expect("source key"),
        json!({ "button": "Blue" }),
        "fence-restart-occurrence",
    );
    let ingress = core
        .tool_intents(
            RESTART_SESSION,
            ExecutionScope::turn(RESTART_SESSION, "fence-restart-turn"),
        )
        .expect("ingress binds");
    let ingress_key = ingress.key("fence-restart-call", 0);
    let start_intent = || {
        lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
            session_id: SessionId::from(RESTART_SESSION.to_string()),
            declaration: start_request().into_declaration(),
        }))
    };
    let start_scope = |label: &str| {
        backend
            .host
            .scoped_static(lash_core::AdmittedScope::runtime_operation(format!(
                "fence-restart-{path:?}-{label}"
            )))
            .expect("runtime operation scope")
            .expect("owned runtime operation scope")
    };

    // The first process, started the way `path` starts work.
    let pruned = match path {
        RegistrationPath::DirectRegistry
        | RegistrationPath::CoreStart
        | RegistrationPath::SessionStart => {
            register_and_complete(backend.registry.as_ref(), Some(RESTART_KEY)).await
        }
        RegistrationPath::TriggerRouter => {
            register_trigger_subscription(
                trigger_store.as_ref(),
                backend.backend.process_env_store().as_ref(),
                &occurrence,
            )
            .await;
            let report = core
                .triggers()
                .emit(occurrence.clone(), start_scope("first"))
                .await
                .expect("the delivery starts its process");
            assert_eq!(
                report.deliveries[0].outcome,
                lash_core::facade_support::TriggerDeliveryEmitOutcome::Started
            );
            report.deliveries[0]
                .process_id
                .clone()
                .expect("a started delivery names its process")
        }
        RegistrationPath::ToolIntentIngress => {
            let outcome = ingress.submit(ingress_key.clone(), start_intent()).await;
            let lash::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { result, .. },
                replayed: false,
            } = outcome
            else {
                panic!("the ingress realizes the start: {outcome:?}");
            };
            lash_core::process_id_from_handle_json(&result)
                .expect("a start answers its process handle")
        }
    };
    if !matches!(
        path,
        RegistrationPath::DirectRegistry
            | RegistrationPath::CoreStart
            | RegistrationPath::SessionStart
    ) {
        // A trigger delivery starts a lash-executed row, which its workflow
        // completes; an ingress start is externally owned.
        let authority = match path {
            RegistrationPath::TriggerRouter => {
                lash_core::ProcessCompletionAuthority::workflow_key("scope-fence-first")
            }
            _ => lash_core::ProcessCompletionAuthority::external_owner(),
        };
        backend
            .registry
            .complete_process(
                &pruned,
                lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(json!("done")),
                ),
                authority,
            )
            .await
            .expect("complete the first process");
    }
    let pruned_scope = ExecutionScope::process(pruned.clone());
    admission(
        backend.host.as_ref(),
        &lash_core::AdmittedScope::process(pruned.clone()),
        "first",
    )
    .await
    .expect("the first process journals");
    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(report.pruned_processes, 1, "{path:?}: the prune took it");
    if let Some(fences) = backend.fence_count(&pruned_scope).await {
        assert_eq!(fences, 1, "{path:?}: the prune left the fence row");
    }

    // The same work, started again.
    let restarted = match path {
        RegistrationPath::DirectRegistry => Some(
            backend
                .registry
                .register_process(external_registration(Some(RESTART_KEY)))
                .await
                .expect("the host starts the work again")
                .id,
        ),
        RegistrationPath::CoreStart => Some(
            core.processes()
                .start(start_request(), start_scope("again"))
                .await
                .expect("the core starts the work again")
                .id,
        ),
        RegistrationPath::SessionStart => {
            let session = core
                .session(RESTART_SESSION)
                .open()
                .await
                .expect("open the session");
            Some(
                session
                    .admin()
                    .processes()
                    .start(start_request(), start_scope("again"))
                    .await
                    .expect("the session starts the work again")
                    .process_id,
            )
        }
        // The prune reclaimed the delivery row with its process, so nothing
        // retains the delivery's key: a redelivery starts a new process. This
        // is the retention gap PR-1 names (FIG-3607); the retention guard that
        // keeps the key past its process lands with the lifetime cutover.
        RegistrationPath::TriggerRouter => {
            let report = core
                .triggers()
                .emit(occurrence.clone(), start_scope("again"))
                .await
                .expect("a redelivered occurrence starts its delivery again");
            assert_eq!(
                report.deliveries[0].outcome,
                lash_core::facade_support::TriggerDeliveryEmitOutcome::Started,
                "{path:?}: a pruned delivery's redelivery starts again"
            );
            Some(
                report.deliveries[0]
                    .process_id
                    .clone()
                    .expect("a started delivery names its process"),
            )
        }
        RegistrationPath::ToolIntentIngress => {
            let outcome = ingress.submit(ingress_key.clone(), start_intent()).await;
            let lash::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { result, .. },
                replayed: true,
            } = outcome
            else {
                panic!("the redrive replays the recorded start: {outcome:?}");
            };
            assert_eq!(
                lash_core::process_id_from_handle_json(&result)
                    .expect("a start answers its process handle"),
                pruned,
                "{path:?}: a redrive after prune returns the recorded id"
            );
            None
        }
    };
    assert_eq!(
        admission(
            backend.host.as_ref(),
            &lash_core::AdmittedScope::process(pruned.clone()),
            "after-restart",
        )
        .await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired),
        "{path:?}: the pruned id stays fenced"
    );
    let err = backend
        .host
        .await_event_key(
            &pruned_scope,
            AwaitEventWaitIdentity::tool_completion("fenced"),
        )
        .await
        .expect_err("a fenced id mints nothing");
    assert_eq!(err.code.as_str(), "await_event_unknown_or_revoked");
    let live = backend
        .registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("list processes");
    let Some(restarted) = restarted else {
        assert!(
            live.is_empty(),
            "{path:?}: nothing was registered for the pruned work: {live:?}"
        );
        return;
    };
    assert_ne!(restarted, pruned, "{path:?}: the restart is a new process");
    assert_eq!(
        live.iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>(),
        vec![restarted.clone()]
    );
    let restarted_scope = ExecutionScope::process(restarted.clone());
    if let Some(fences) = backend.fence_count(&restarted_scope).await {
        assert_eq!(fences, 0, "{path:?}: the new process's scope is unfenced");
    }
    admission(
        backend.host.as_ref(),
        &lash_core::AdmittedScope::process(restarted.clone()),
        "restarted",
    )
    .await
    .unwrap_or_else(|err| panic!("{path:?}: the new process journals: {err:?}"));
    let key = backend
        .host
        .await_event_key(
            &restarted_scope,
            AwaitEventWaitIdentity::tool_completion("restarted"),
        )
        .await
        .expect("the new process mints");
    assert_eq!(
        backend
            .host
            .resolve_await_event(&key, Resolution::Ok(json!("second")))
            .await
            .expect("resolve"),
        lash_core::ResolveOutcome::Accepted
    );
}

macro_rules! restart_tests {
    ($($name:ident => ($kind:ident, $path:ident)),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                restarting_pruned_work(Kind::$kind, RegistrationPath::$path).await;
            }
        )*
    };
}

restart_tests! {
    sqlite_direct_registration_restart_is_a_new_process => (Sqlite, DirectRegistry),
    sqlite_core_start_restart_is_a_new_process => (Sqlite, CoreStart),
    sqlite_session_start_restart_is_a_new_process => (Sqlite, SessionStart),
    sqlite_trigger_redelivery_after_prune_is_a_new_process => (Sqlite, TriggerRouter),
    sqlite_tool_intent_redrive_returns_the_recorded_id => (Sqlite, ToolIntentIngress),
}

fn sqlite_fence_rows(path: impl AsRef<std::path::Path>, scope_id: &str) -> i64 {
    sqlite_count(
        path,
        "SELECT COUNT(*) FROM effect_scope_retirements WHERE scope_id = ?1",
        scope_id,
    )
}

fn sqlite_count(path: impl AsRef<std::path::Path>, sql: &str, argument: &str) -> i64 {
    rusqlite::Connection::open(path.as_ref())
        .expect("open the SQLite file")
        .query_row(sql, [argument], |row| row.get(0))
        .expect("count rows")
}

fn sqlite_execute(path: &std::path::Path, sql: &str, argument: &str) {
    rusqlite::Connection::open(path)
        .expect("open the SQLite file")
        .execute(sql, [argument])
        .expect("execute");
}

fn scope_key(scope: &ExecutionScope) -> String {
    scope
        .journal_identity()
        .expect("process journal identity")
        .key()
        .to_string()
}

/// Retirement commits the fence into the registry file first; the journal
/// purge is a second transaction on the journal file. A crash between the
/// two leaves a fenced scope with stale journal rows: a cold host refuses
/// admission under it and repairs the rows when the registry binds, and the
/// reclaim sweep purges them too (FIG-2499 fix round 3, ruling 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_fence_committed_before_a_lost_journal_purge_refuses_cold_admission_and_is_purged() {
    let backend = backend(Kind::Sqlite).await.expect("SQLite backend");
    let (registry_path, journal_path) = backend.sqlite_files.clone().expect("SQLite files");
    let factory = backend
        .sqlite
        .as_ref()
        .expect("SQLite backend")
        .session_store_factory();
    let core = core_over(backend.backend.clone());
    // The catalog the sweep opens exists once a session has been created.
    let session = core
        .session("purge-lost-session")
        .open()
        .await
        .expect("session");

    let journal_rows = |key: &str| {
        sqlite_count(
            &journal_path,
            "SELECT COUNT(*) FROM runtime_effect_replay WHERE scope_id = ?1",
            key,
        )
    };
    let mut keys = Vec::new();
    for _ in 0..2 {
        let process_id = register_and_complete(backend.registry.as_ref(), None).await;
        let scope = ExecutionScope::process(process_id.clone());
        admission(
            backend.host.as_ref(),
            &lash_core::AdmittedScope::process(process_id.clone()),
            "journaled-before-retirement",
        )
        .await
        .expect("the process journals");
        let key = scope_key(&scope);
        assert_eq!(journal_rows(&key), 1);
        // The prune committed and the retirement's first transaction — the
        // fence in the registry file — committed; the journal purge was lost.
        sqlite_execute(
            &registry_path,
            "DELETE FROM processes WHERE process_id = ?1",
            process_id.as_str(),
        );
        sqlite_execute(
            &registry_path,
            "INSERT INTO effect_scope_retirements (scope_id, retired_at_ms) VALUES (?1, 0)",
            &key,
        );
        assert_eq!(journal_rows(&key), 1, "the stale rows survived the cut");
        keys.push((scope, key));
    }
    drop(session);
    drop(core);

    // The sweep purges the rows under the committed fence.
    let (_, swept_key) = &keys[0];
    let report = factory
        .reclaim_retained_evidence(lash::persistence::RetentionBound {
            committed_before_epoch_ms: 0,
        })
        .await
        .expect("the sweep commits");
    assert_eq!(
        journal_rows(swept_key),
        0,
        "the sweep purged the rows under the fenced scope: {report:?}"
    );
    assert_eq!(sqlite_fence_rows(&registry_path, swept_key), 1);

    // A cold host refuses the fenced scope and repairs its rows at bind.
    let (bound_scope, bound_key) = &keys[1];
    let cold = (backend.cold_host.as_ref().expect("durable backend"))().await;
    assert_eq!(
        admission(
            cold.as_ref(),
            &lash_core::AdmittedScope::new(bound_scope.clone()),
            "after-lost-purge",
        )
        .await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired),
        "the committed fence refuses admission whatever the journal still holds"
    );
    let minted = cold
        .await_event_key(
            bound_scope,
            AwaitEventWaitIdentity::tool_completion("after-lost-purge"),
        )
        .await
        .expect_err("the committed fence refuses the mint");
    assert_eq!(minted.code.as_str(), "await_event_unknown_or_revoked");
    assert_eq!(
        journal_rows(bound_key),
        0,
        "binding the registry purged the stale rows"
    );
    assert_eq!(sqlite_fence_rows(&registry_path, bound_key), 1);
}
