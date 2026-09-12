//! The `process:{id}` scope fence around a process prune: only rows the
//! registry actually prunes are fenced (watermark and pending-work guards
//! are honoured), a pruned id stays fenced until the host registers it
//! again, and registration lifts the fence so the new incarnation starts
//! unfenced with the empty journal the prune left (FIG-2499, ADR 0049).

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use std::sync::Arc;

use lash::LashCore;
use lash::durability::EffectHost;
use lash::persistence::SessionStoreFactory as _;
use lash_core::ProcessRegistrar as _;
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

fn core_with(
    effect_host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
) -> LashCore {
    core_with_triggers(
        effect_host,
        registry,
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default()),
    )
}

fn core_with_triggers(
    effect_host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
) -> LashCore {
    core_with_all(
        effect_host,
        registry,
        trigger_store,
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    )
}

fn core_with_store(
    effect_host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
) -> LashCore {
    core_with_all(
        effect_host,
        registry,
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default()),
        store_factory,
    )
}

fn core_with_all(
    effect_host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
) -> LashCore {
    let (process_env_store, _) = lash_core::testing::process_execution_env_fixture();
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
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .model(
            lash::ModelSpec::builder("mock-model")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .store_factory(store_factory)
        .effect_host(effect_host)
        .process_registry(registry)
        .trigger_store(trigger_store)
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(process_env_store)
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

/// One backend's effect host and process registry, plus the row observations
/// the durable backends expose.
struct Backend {
    host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    fences: Option<FenceCounter>,
    /// Where a fresh host over the same durable journal comes from: a cold
    /// observer proves persisted state rather than a warm fence cache.
    cold_host: Option<ColdHost>,
    /// The SQLite registry database, for failure injection at the insert.
    sqlite_registry: Option<std::path::PathBuf>,
    /// The SQLite effect journal, for the two-file layout witnesses.
    sqlite_journal: Option<std::path::PathBuf>,
    _dir: tempfile::TempDir,
    _postgres: Option<lash_postgres_store::PostgresStorage>,
}

enum Kind {
    Memory,
    Sqlite,
    Postgres,
}

async fn backend(kind: Kind) -> Option<Backend> {
    let dir = tempfile::tempdir().expect("tempdir");
    Some(match kind {
        Kind::Memory => Backend {
            host: Arc::new(lash_core::facade_support::NativeEffectHost::default()),
            registry: Arc::new(lash_core::TestLocalProcessRegistry::default()),
            fences: None,
            cold_host: None,
            sqlite_registry: None,
            sqlite_journal: None,
            _dir: dir,
            _postgres: None,
        },
        Kind::Sqlite => {
            let path = dir.path().join("effect.db");
            let host = lash_sqlite_store::SqliteEffectHost::open(&path)
                .await
                .expect("SQLite effect host");
            let registry_path = dir.path().join("registry.db");
            let registry = lash_sqlite_store::SqliteProcessRegistry::open(
                &registry_path,
                dir.path().join("sessions"),
            )
            .await
            .expect("SQLite process registry");
            let fence_journal = path.clone();
            let fence_registry = registry_path.clone();
            let cold_path = path.clone();
            let cold_registry = registry_path.clone();
            let cold_sessions = dir.path().join("sessions");
            Backend {
                host: Arc::new(host),
                registry: Arc::new(registry),
                cold_host: Some(Box::new(move || {
                    let path = cold_path.clone();
                    let registry_path = cold_registry.clone();
                    let sessions = cold_sessions.clone();
                    Box::pin(async move {
                        let host = Arc::new(
                            lash_sqlite_store::SqliteEffectHost::open(&path)
                                .await
                                .expect("cold SQLite effect host"),
                        ) as Arc<dyn EffectHost>;
                        // A registered process's fence lives in the registry
                        // file; a cold host reads it once the registry binds
                        // it, which `LashCore::build` does on every open.
                        let registry = lash_sqlite_store::SqliteProcessRegistry::open(
                            &registry_path,
                            sessions,
                        )
                        .await
                        .expect("cold SQLite process registry");
                        registry.bind_effect_host(&host);
                        host
                    })
                })),
                sqlite_registry: Some(registry_path),
                sqlite_journal: Some(path),
                fences: Some(Box::new(move |scope_id: &str| {
                    let journal = fence_journal.clone();
                    let registry = fence_registry.clone();
                    let scope_id = scope_id.to_string();
                    Box::pin(async move {
                        // The fence is one row in one of the two files:
                        // count both so the witness reads the layout, not
                        // an assumption about which file holds it.
                        sqlite_fence_rows(&journal, &scope_id)
                            + sqlite_fence_rows(&registry, &scope_id)
                    })
                })),
                _dir: dir,
                _postgres: None,
            }
        }
        Kind::Postgres => {
            let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
                assert!(
                    std::env::var("LASH_REQUIRE_POSTGRES").is_err(),
                    "LASH_REQUIRE_POSTGRES=1 but LASH_POSTGRES_DATABASE_URL is not set"
                );
                eprintln!(
                    "skipping Postgres process fence test: LASH_POSTGRES_DATABASE_URL is not set"
                );
                return None;
            };
            let admin = sqlx::PgPool::connect(&url).await.expect("connect postgres");
            let name = format!("process_fence_{}", uuid::Uuid::new_v4().simple());
            sqlx::query(&format!("CREATE DATABASE {name}"))
                .execute(&admin)
                .await
                .expect("create a private database");
            admin.close().await;
            let (base, _) = url.rsplit_once('/').expect("database url has a path");
            let storage = lash_postgres_store::PostgresStorage::connect(&format!("{base}/{name}"))
                .await
                .expect("connect the private database");
            let pool = storage.pool().clone();
            let cold_storage = storage.clone();
            Backend {
                host: Arc::new(storage.effect_host()),
                registry: Arc::new(storage.process_registry()),
                cold_host: Some(Box::new(move || {
                    let storage = cold_storage.clone();
                    Box::pin(async move { Arc::new(storage.effect_host()) as Arc<dyn EffectHost> })
                })),
                sqlite_registry: None,
                sqlite_journal: None,
                fences: Some(Box::new(move |scope_id: &str| {
                    let pool = pool.clone();
                    let scope_id = scope_id.to_string();
                    Box::pin(async move {
                        sqlx::query_scalar(
                            "SELECT COUNT(*) FROM lash_effect_scope_retirements WHERE scope_id = $1",
                        )
                        .bind(scope_id)
                        .fetch_one(&pool)
                        .await
                        .expect("count fences")
                    })
                })),
                _dir: dir,
                _postgres: Some(storage),
            }
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

fn external_registration(process_id: &ProcessId) -> lash_core::ProcessRegistration {
    lash_core::ProcessRegistration::new(
        process_id,
        lash_core::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessProvenance::host(),
    )
    .with_identity(lash_core::ProcessIdentity::new("test"))
}

async fn register_and_complete(registry: &dyn lash_core::ProcessRegistry, process_id: &ProcessId) {
    registry
        .register_process(external_registration(process_id))
        .await
        .expect("register the process");
    registry
        .complete_process(
            process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                json!("done"),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete the process");
}

async fn admission(
    host: &dyn EffectHost,
    scope: &ExecutionScope,
    effect_id: &str,
) -> Result<(), lash_core::RuntimeErrorCode> {
    host.scoped(scope.clone())
        .expect("scope binds")
        .controller()
        .execute_effect(envelope(scope, effect_id), executor())
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
    let core = core_with(Arc::clone(&backend.host), Arc::clone(&backend.registry));
    let process_id = "retained-by-watermark";
    register_and_complete(backend.registry.as_ref(), &ProcessId::from(process_id)).await;
    let scope = ExecutionScope::process(process_id);
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
            .get_process(&ProcessId::from(process_id))
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
    admission(backend.host.as_ref(), &scope, "still-admitted")
        .await
        .expect("an unpruned process is still admitted");

    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune without a projector");
    assert_eq!(report.pruned_processes, 1);
    assert_eq!(
        admission(backend.host.as_ref(), &scope, "after-prune").await,
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

/// A pruned process id stays fenced until the host registers it again through
/// the facade; registration lifts the fence and the new incarnation claims
/// and mints again (ADR 0049).
async fn pruned_process_id_is_fenced_until_registered_again(kind: Kind) {
    let Some(backend) = backend(kind).await else {
        return;
    };
    let core = core_with(Arc::clone(&backend.host), Arc::clone(&backend.registry));
    let process_id = "reused-by-host";
    register_and_complete(backend.registry.as_ref(), &ProcessId::from(process_id)).await;
    let scope = ExecutionScope::process(process_id);
    admission(backend.host.as_ref(), &scope, "first-incarnation")
        .await
        .expect("the first incarnation journals");

    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(report.pruned_processes, 1);
    assert_eq!(
        admission(
            backend.host.as_ref(),
            &scope,
            "between-prune-and-reregistration"
        )
        .await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired),
        "the interval between prune and re-registration is fenced"
    );
    let err = backend
        .host
        .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("fenced"))
        .await
        .expect_err("a fenced id mints nothing");
    assert_eq!(err.code.as_str(), "await_event_unknown_or_revoked");
    if let Some(fences) = backend.fence_count(&scope).await {
        assert_eq!(fences, 1, "the prune left the fence row");
    }

    let start_scope = backend
        .host
        .scoped_static(ExecutionScope::runtime_operation("reregister-start"))
        .expect("runtime operation scope")
        .expect("owned runtime operation scope");
    let record = core
        .processes()
        .start(
            lash_core::ProcessStartRequest::new(
                process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessOriginator::host(),
            ),
            start_scope,
        )
        .await
        .expect("the host registers the pruned id again");
    assert_eq!(record.id, process_id);

    if let Some(fences) = backend.fence_count(&scope).await {
        assert_eq!(fences, 0, "registration cleared the fence row");
    }
    admission(backend.host.as_ref(), &scope, "second-incarnation")
        .await
        .expect("the re-registered incarnation claims");
    let key = backend
        .host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("reinstated"),
        )
        .await
        .expect("the re-registered incarnation mints");
    assert_eq!(
        backend
            .host
            .resolve_await_event(&key, Resolution::Ok(json!("second")))
            .await
            .expect("resolve"),
        lash_core::ResolveOutcome::Accepted
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_prune_fences_only_what_the_registry_prunes() {
    prune_fences_only_what_the_registry_prunes(Kind::Memory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_prune_fences_only_what_the_registry_prunes() {
    prune_fences_only_what_the_registry_prunes(Kind::Sqlite).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_prune_fences_only_what_the_registry_prunes() {
    prune_fences_only_what_the_registry_prunes(Kind::Postgres).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_pruned_process_id_is_fenced_until_registered_again() {
    pruned_process_id_is_fenced_until_registered_again(Kind::Memory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_pruned_process_id_is_fenced_until_registered_again() {
    pruned_process_id_is_fenced_until_registered_again(Kind::Sqlite).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_pruned_process_id_is_fenced_until_registered_again() {
    pruned_process_id_is_fenced_until_registered_again(Kind::Postgres).await;
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

const REUSE_SESSION: &str = "fence-reuse-session";

fn start_request(process_id: &ProcessId) -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        process_id,
        lash_core::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessOriginator::host(),
    )
}

/// Registers a trigger subscription on the core's store and returns the
/// process id the router will hand the delivery of `occurrence`.
async fn trigger_delivery_process_id(
    store: &dyn lash_core::TriggerStore,
    occurrence: &lash_core::TriggerOccurrenceRequest,
) -> String {
    let (_, process_env_ref) = lash_core::testing::process_execution_env_fixture();
    let draft = lash_core::TriggerSubscriptionDraft::for_process(
        "test/fence-reuse",
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
            "fence-reuse-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("fence-test").expect("host scope"),
                actor: lash_core::ProcessOriginator::host_scoped("fence-test"),
                draft,
            },
        )
        .await
        .expect("execute the registration")
        .expect("register the subscription");
    let lash_core::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("expected a mutation receipt");
    };
    let record = receipt.record_snapshot;
    lash_core::facade_support::deterministic_delivery_process_id(
        &lash_core::facade_support::deterministic_occurrence_id(occurrence),
        &record.subscription_id,
        &record.incarnation,
        record.revision,
    )
    .expect("delivery process id")
    .to_string()
}

/// A pruned process id is fenced until it is registered again, whichever
/// registrant performs the registration: the fence is released by the
/// registry insert itself, so every path that ends there — direct
/// registration, `Processes::start`, a session-scoped start, a trigger
/// delivery and a tool-intent `StartProcess` — leaves the new incarnation
/// unfenced with its first effect admitted (FIG-2499 fix round 2, ruling 1).
async fn registration_path_lifts_the_fence(kind: Kind, path: RegistrationPath) {
    let Some(backend) = backend(kind).await else {
        return;
    };
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let core = core_with_triggers(
        Arc::clone(&backend.host),
        Arc::clone(&backend.registry),
        Arc::clone(&trigger_store),
    );
    let occurrence = lash_core::TriggerOccurrenceRequest::new(
        "ui.button.pressed",
        lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
            .expect("source key"),
        json!({ "button": "Blue" }),
        "fence-reuse-occurrence",
    );
    let ingress = core
        .tool_intents(
            REUSE_SESSION,
            ExecutionScope::turn(REUSE_SESSION, "fence-reuse-turn"),
        )
        .expect("ingress binds");
    let ingress_key = ingress.key("fence-reuse-call", 0);
    let process_id = match path {
        RegistrationPath::DirectRegistry => "reused-direct".to_string(),
        RegistrationPath::CoreStart => "reused-core-start".to_string(),
        RegistrationPath::SessionStart => "reused-session-start".to_string(),
        RegistrationPath::TriggerRouter => {
            trigger_delivery_process_id(trigger_store.as_ref(), &occurrence).await
        }
        RegistrationPath::ToolIntentIngress => ingress_key.identity().replay_key.clone(),
    };
    register_and_complete(
        backend.registry.as_ref(),
        &ProcessId::from(process_id.clone()),
    )
    .await;
    let scope = ExecutionScope::process(&process_id);
    admission(backend.host.as_ref(), &scope, "first-incarnation")
        .await
        .expect("the first incarnation journals");
    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(
        report.pruned_processes, 1,
        "{path:?}: the prune took the process"
    );
    assert_eq!(
        admission(backend.host.as_ref(), &scope, "while-pruned").await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired),
        "{path:?}: the pruned id is fenced"
    );
    if let Some(fences) = backend.fence_count(&scope).await {
        assert_eq!(fences, 1, "{path:?}: the prune left the fence row");
    }

    let start_scope = || {
        backend
            .host
            .scoped_static(ExecutionScope::runtime_operation(format!(
                "fence-reuse-{path:?}"
            )))
            .expect("runtime operation scope")
            .expect("owned runtime operation scope")
    };
    match path {
        RegistrationPath::DirectRegistry => {
            backend
                .registry
                .register_process(external_registration(&ProcessId::from(process_id.clone())))
                .await
                .expect("the host registers the pruned id again");
        }
        RegistrationPath::CoreStart => {
            let record = core
                .processes()
                .start(
                    start_request(&ProcessId::from(process_id.clone())),
                    start_scope(),
                )
                .await
                .expect("the core registers the pruned id again");
            assert_eq!(record.id, process_id);
        }
        RegistrationPath::SessionStart => {
            let session = core
                .session(REUSE_SESSION)
                .open()
                .await
                .expect("open the session");
            let view = session
                .admin()
                .processes()
                .start(
                    start_request(&ProcessId::from(process_id.clone())),
                    start_scope(),
                )
                .await
                .expect("the session registers the pruned id again");
            assert_eq!(view.id, process_id);
        }
        RegistrationPath::TriggerRouter => {
            let report = core
                .triggers()
                .emit(occurrence.clone(), start_scope())
                .await
                .expect("the trigger delivery registers the pruned id again");
            assert_eq!(report.deliveries.len(), 1);
            assert_eq!(report.deliveries[0].process_id, process_id);
            assert_eq!(
                report.deliveries[0].outcome,
                lash_core::facade_support::TriggerDeliveryEmitOutcome::Started
            );
        }
        RegistrationPath::ToolIntentIngress => {
            let outcome = ingress
                .submit(
                    ingress_key.clone(),
                    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
                        session_id: SessionId::from(REUSE_SESSION.to_string()),
                        request: start_request(&ProcessId::from("host-chosen-id-is-replaced")),
                        on_parent_end: Default::default(),
                    })),
                )
                .await;
            let lash::tools::ToolIntentIngressOutcome::Admitted { replayed, .. } = outcome else {
                panic!("the ingress admits the start: {outcome:?}");
            };
            assert!(!replayed, "the start is realized, not replayed");
        }
    }
    assert!(
        backend
            .registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read the process")
            .is_some(),
        "{path:?}: the id is registered again"
    );
    if let Some(fences) = backend.fence_count(&scope).await {
        assert_eq!(
            fences, 0,
            "{path:?}: the registry insert cleared the fence row"
        );
    }
    admission(backend.host.as_ref(), &scope, "second-incarnation")
        .await
        .unwrap_or_else(|err| panic!("{path:?}: the re-registered incarnation claims: {err:?}"));
    let key = backend
        .host
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("reinstated"),
        )
        .await
        .expect("the re-registered incarnation mints");
    assert_eq!(
        backend
            .host
            .resolve_await_event(&key, Resolution::Ok(json!("second")))
            .await
            .expect("resolve"),
        lash_core::ResolveOutcome::Accepted
    );
}

/// A registration that fails at the registry insert rolls the fence release
/// back with it: the pruned id stays fenced and unregistered, and a cold host
/// over the same journal admits nothing under it (astra delta probe, FIG-2499
/// fix round 2, ruling 1).
async fn failed_registration_keeps_the_fence(kind: Kind) {
    let Some(backend) = backend(kind).await else {
        return;
    };
    let core = core_with(Arc::clone(&backend.host), Arc::clone(&backend.registry));
    let process_id = "reused-but-insert-fails";
    register_and_complete(backend.registry.as_ref(), &ProcessId::from(process_id)).await;
    let scope = ExecutionScope::process(process_id);
    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(report.pruned_processes, 1);
    assert_eq!(backend.fence_count(&scope).await, Some(1));

    // Fail the registry insert itself, after the fence delete in the same
    // transaction has run.
    if let Some(registry_path) = &backend.sqlite_registry {
        rusqlite::Connection::open(registry_path)
            .expect("open the registry")
            .execute_batch(
                "CREATE TRIGGER injected_start BEFORE INSERT ON processes BEGIN \
                 SELECT RAISE(ABORT, 'injected registration failure'); END;",
            )
            .expect("install the trigger");
    } else {
        let pool = backend
            ._postgres
            .as_ref()
            .expect("postgres backend")
            .pool()
            .clone();
        sqlx::raw_sql(
            "CREATE FUNCTION injected_fail_start() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'injected registration failure'; END $$; \
             CREATE TRIGGER injected_start BEFORE INSERT ON lash_processes \
             FOR EACH ROW EXECUTE FUNCTION injected_fail_start();",
        )
        .execute(&pool)
        .await
        .expect("install the trigger");
    }
    let start_scope = backend
        .host
        .scoped_static(ExecutionScope::runtime_operation("failing-start"))
        .expect("runtime operation scope")
        .expect("owned runtime operation scope");
    let err = core
        .processes()
        .start(start_request(&ProcessId::from(process_id)), start_scope)
        .await
        .expect_err("the injected failure reaches the caller");
    assert!(
        err.to_string().contains("injected registration failure"),
        "the failure is the injected insert failure: {err}"
    );

    let lookup = backend
        .registry
        .get_process(&ProcessId::from(process_id))
        .await;
    assert!(
        matches!(
            lookup,
            Ok(None) | Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the failed insert registered nothing: {lookup:?}"
    );
    assert_eq!(
        backend.fence_count(&scope).await,
        Some(1),
        "the fence release rolled back with the insert"
    );
    let cold = (backend.cold_host.as_ref().expect("durable backend"))().await;
    assert_eq!(
        admission(cold.as_ref(), &scope, "stale-redrive").await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired),
        "a cold host admits nothing under the still-fenced id"
    );
    let minted = cold
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("stale-wait"),
        )
        .await
        .expect_err("a cold host mints nothing under the still-fenced id");
    assert_eq!(minted.code.as_str(), "await_event_unknown_or_revoked");
}

/// The registry drives every bound host's scope reinstatement from the
/// registration seam — the same seam the Restate host's index reinstate
/// hangs off — so a host whose fence lives outside the registry's store is
/// lifted by the same insert (FIG-2499 fix round 2, ruling 1).
async fn registration_reinstates_every_bound_host(kind: Kind) {
    let Some(backend) = backend(kind).await else {
        return;
    };
    let _core = core_with(Arc::clone(&backend.host), Arc::clone(&backend.registry));
    let other: Arc<dyn EffectHost> =
        Arc::new(lash_core::facade_support::NativeEffectHost::default());
    backend.registry.bind_effect_host(&other);
    let process_id = "reused-across-hosts";
    let scope = ExecutionScope::process(process_id);
    other
        .retire_effect_journal(lash::durability::EffectJournalRetirement::process(
            process_id,
        ))
        .await
        .expect("the other host fences the id");
    assert_eq!(
        admission(other.as_ref(), &scope, "while-fenced").await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired)
    );
    backend
        .registry
        .register_process(external_registration(&ProcessId::from(process_id)))
        .await
        .expect("register the id");
    admission(other.as_ref(), &scope, "after-registration")
        .await
        .expect("the registration seam reinstated the bound host's scope");
}

macro_rules! registration_path_tests {
    ($($name:ident => ($kind:ident, $path:ident)),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                registration_path_lifts_the_fence(Kind::$kind, RegistrationPath::$path).await;
            }
        )*
    };
}

registration_path_tests! {
    memory_direct_registration_lifts_the_fence => (Memory, DirectRegistry),
    memory_core_start_lifts_the_fence => (Memory, CoreStart),
    memory_session_start_lifts_the_fence => (Memory, SessionStart),
    memory_trigger_delivery_lifts_the_fence => (Memory, TriggerRouter),
    memory_tool_intent_ingress_lifts_the_fence => (Memory, ToolIntentIngress),
    sqlite_direct_registration_lifts_the_fence => (Sqlite, DirectRegistry),
    sqlite_core_start_lifts_the_fence => (Sqlite, CoreStart),
    sqlite_session_start_lifts_the_fence => (Sqlite, SessionStart),
    sqlite_trigger_delivery_lifts_the_fence => (Sqlite, TriggerRouter),
    sqlite_tool_intent_ingress_lifts_the_fence => (Sqlite, ToolIntentIngress),
    postgres_direct_registration_lifts_the_fence => (Postgres, DirectRegistry),
    postgres_core_start_lifts_the_fence => (Postgres, CoreStart),
    postgres_session_start_lifts_the_fence => (Postgres, SessionStart),
    postgres_trigger_delivery_lifts_the_fence => (Postgres, TriggerRouter),
    postgres_tool_intent_ingress_lifts_the_fence => (Postgres, ToolIntentIngress),
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_failed_registration_keeps_the_fence() {
    failed_registration_keeps_the_fence(Kind::Sqlite).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_failed_registration_keeps_the_fence() {
    failed_registration_keeps_the_fence(Kind::Postgres).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_registration_reinstates_every_bound_host() {
    registration_reinstates_every_bound_host(Kind::Memory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_registration_reinstates_every_bound_host() {
    registration_reinstates_every_bound_host(Kind::Sqlite).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_registration_reinstates_every_bound_host() {
    registration_reinstates_every_bound_host(Kind::Postgres).await;
}

fn sqlite_fence_rows(path: &std::path::Path, scope_id: &str) -> i64 {
    sqlite_count(
        path,
        "SELECT COUNT(*) FROM effect_scope_retirements WHERE scope_id = ?1",
        scope_id,
    )
}

fn sqlite_count(path: &std::path::Path, sql: &str, argument: &str) -> i64 {
    rusqlite::Connection::open(path)
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

/// Runs `statements` against `path` inside one `BEGIN IMMEDIATE` transaction
/// in a child process that kills itself with SIGKILL right after the last
/// statement: with `commit` the transaction is durable when the process dies,
/// without it the transaction is lost with the process. A real crash cut, at
/// the granularity SQLite's WAL commits — one file (ADR 0049).
fn crash_after(path: &std::path::Path, statements: &[String], commit: bool) {
    use std::os::unix::process::ExitStatusExt;
    let script = "import sqlite3, sys, os, signal\n\
                  c = sqlite3.connect(sys.argv[1], isolation_level=None)\n\
                  c.execute('BEGIN IMMEDIATE')\n\
                  for statement in sys.argv[3:]:\n\
                  \x20   c.execute(statement)\n\
                  if sys.argv[2] == 'commit':\n\
                  \x20   c.execute('COMMIT')\n\
                  os.kill(os.getpid(), signal.SIGKILL)\n";
    let status = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(path)
        .arg(if commit { "commit" } else { "cut" })
        .args(statements)
        .status()
        .expect("spawn the crashing writer");
    assert_eq!(status.signal(), Some(9), "the writer died by SIGKILL");
}

/// A pruned process id's fence and its registry row live in the same file,
/// so registration — the fence delete and the process insert — is one
/// single-file transaction. A crash cut after that transaction leaves the id
/// registered and unfenced; a cut before it leaves the id fenced and
/// unregistered; the journal file takes no part, so "unfenced and
/// unregistered" is not a state a cut can produce (FIG-2499 fix round 3,
/// ruling 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_registration_crash_cut_leaves_the_id_fenced_or_registered_never_neither() {
    let backend = backend(Kind::Sqlite).await.expect("SQLite backend");
    let registry_path = backend.sqlite_registry.clone().expect("registry file");
    let journal_path = backend.sqlite_journal.clone().expect("journal file");
    let core = core_with(Arc::clone(&backend.host), Arc::clone(&backend.registry));
    let process_id = "crash-cut";
    let scope = ExecutionScope::process(process_id);
    let key = scope_key(&scope);
    register_and_complete(backend.registry.as_ref(), &ProcessId::from(process_id)).await;
    admission(backend.host.as_ref(), &scope, "first-incarnation")
        .await
        .expect("the first incarnation journals");
    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(report.pruned_processes, 1);
    assert_eq!(
        sqlite_fence_rows(&registry_path, &key),
        1,
        "the fence of a process under a bound registry is a registry-file row"
    );
    assert_eq!(
        sqlite_fence_rows(&journal_path, &key),
        0,
        "the journal file holds no copy of it: there is nothing for a journal-only commit to lose"
    );

    // Capture an authentic registration row, then put the pre-registration
    // state back so the crashing writer can replay the registration itself.
    backend
        .registry
        .register_process(external_registration(&ProcessId::from(process_id)))
        .await
        .expect("register the pruned id once to capture its row");
    let (columns, values) = {
        let conn = rusqlite::Connection::open(&registry_path).expect("open the registry");
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(processes)")
            .expect("describe processes")
            .query_map([], |row| row.get(1))
            .expect("columns")
            .collect::<Result<_, _>>()
            .expect("column names");
        let values: Vec<String> = columns
            .iter()
            .map(|column| {
                conn.query_row(
                    &format!("SELECT quote({column}) FROM processes WHERE process_id = ?1"),
                    [process_id],
                    |row| row.get(0),
                )
                .expect("quoted value")
            })
            .collect();
        (columns, values)
    };
    let registration = vec![
        format!("DELETE FROM effect_scope_retirements WHERE scope_id = '{key}'"),
        format!(
            "INSERT INTO processes ({}) VALUES ({})",
            columns.join(", "),
            values.join(", ")
        ),
    ];
    let reset = || {
        sqlite_execute(
            &registry_path,
            "DELETE FROM processes WHERE process_id = ?1",
            process_id,
        );
        sqlite_execute(
            &registry_path,
            "INSERT OR IGNORE INTO effect_scope_retirements (scope_id, retired_at_ms) VALUES (?1, 0)",
            &key,
        );
    };
    let registered = || {
        sqlite_count(
            &registry_path,
            "SELECT COUNT(*) FROM processes WHERE process_id = ?1",
            process_id,
        )
    };
    drop(core);

    // Cut after the commit: registered and unfenced, and the cold host runs
    // the new incarnation's work.
    reset();
    crash_after(&registry_path, &registration, true);
    assert_eq!(registered(), 1);
    assert_eq!(sqlite_fence_rows(&registry_path, &key), 0);
    assert_eq!(sqlite_fence_rows(&journal_path, &key), 0);
    let cold = (backend.cold_host.as_ref().expect("durable backend"))().await;
    admission(cold.as_ref(), &scope, "after-committed-cut")
        .await
        .expect("a registered, unfenced id is admitted after the cut");
    cold.await_event_key(
        &scope,
        AwaitEventWaitIdentity::tool_completion("after-committed-cut"),
    )
    .await
    .expect("a registered, unfenced id mints after the cut");
    drop(cold);

    // Cut before the commit: fenced and unregistered, and the cold host admits
    // nothing under the id.
    reset();
    crash_after(&registry_path, &registration, false);
    assert_eq!(registered(), 0);
    assert_eq!(sqlite_fence_rows(&registry_path, &key), 1);
    assert_eq!(sqlite_fence_rows(&journal_path, &key), 0);
    let cold = (backend.cold_host.as_ref().expect("durable backend"))().await;
    assert_eq!(
        admission(cold.as_ref(), &scope, "after-lost-cut").await,
        Err(lash_core::RuntimeErrorCode::EffectScopeRetired),
        "a fenced, unregistered id admits nothing after the cut"
    );
    let minted = cold
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("after-lost-cut"),
        )
        .await
        .expect_err("a fenced, unregistered id mints nothing after the cut");
    assert_eq!(minted.code.as_str(), "await_event_unknown_or_revoked");
}

/// Retirement commits the fence into the registry file first; the journal
/// purge is a second transaction on the journal file. A crash between the
/// two leaves a fenced scope with stale journal rows: a cold host refuses
/// admission under it and repairs the rows when the registry binds, and the
/// reclaim sweep purges them too (FIG-2499 fix round 3, ruling 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_fence_committed_before_a_lost_journal_purge_refuses_cold_admission_and_is_purged() {
    let backend = backend(Kind::Sqlite).await.expect("SQLite backend");
    let registry_path = backend.sqlite_registry.clone().expect("registry file");
    let journal_path = backend.sqlite_journal.clone().expect("journal file");
    let factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(
            backend._dir.path().join("catalog"),
            registry_path.clone(),
        ),
    );
    let core = core_with_store(
        Arc::clone(&backend.host),
        Arc::clone(&backend.registry),
        Arc::clone(&factory) as Arc<dyn lash::persistence::SessionStoreFactory>,
    );
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
    for process_id in ["purge-lost-swept", "purge-lost-bound"] {
        let scope = ExecutionScope::process(process_id);
        register_and_complete(backend.registry.as_ref(), &ProcessId::from(process_id)).await;
        admission(backend.host.as_ref(), &scope, "journaled-before-retirement")
            .await
            .expect("the process journals");
        let key = scope_key(&scope);
        assert_eq!(journal_rows(&key), 1);
        // The prune committed and the retirement's first transaction — the
        // fence in the registry file — committed; the journal purge was lost.
        sqlite_execute(
            &registry_path,
            "DELETE FROM processes WHERE process_id = ?1",
            process_id,
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
        admission(cold.as_ref(), bound_scope, "after-lost-purge").await,
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
