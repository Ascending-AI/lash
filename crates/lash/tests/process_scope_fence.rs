//! The `process:{id}` scope fence around a process prune: only rows the
//! registry actually prunes are fenced (watermark and pending-work guards
//! are honoured), a pruned id stays fenced until the host registers it
//! again, and registration lifts the fence so the new incarnation starts
//! unfenced with the empty journal the prune left (FIG-2499, ADR 0049).

use std::sync::Arc;

use lash::LashCore;
use lash::durability::EffectHost;
use lash_core::{
    AwaitEventWaitIdentity, ExecutionScope, Resolution, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeInvocation, RuntimeScope,
};
use serde_json::json;

fn envelope(effect_id: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("fence-session", "fence-turn", 1, 0),
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
            value: json!({ "ran": true }),
        })
    })
}

fn core_with(
    effect_host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
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
    LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(provider)
        .model(
            lash::ModelSpec::builder("mock-model")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .effect_host(effect_host)
        .process_registry(registry)
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
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

/// One backend's effect host and process registry, plus the row observations
/// the durable backends expose.
struct Backend {
    host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    fences: Option<FenceCounter>,
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
            _dir: dir,
            _postgres: None,
        },
        Kind::Sqlite => {
            let path = dir.path().join("effect.db");
            let host = lash_sqlite_store::SqliteEffectHost::open(&path)
                .await
                .expect("SQLite effect host");
            let registry = lash_sqlite_store::SqliteProcessRegistry::open(
                &dir.path().join("registry.db"),
                dir.path().join("sessions"),
            )
            .await
            .expect("SQLite process registry");
            let fence_path = path.clone();
            Backend {
                host: Arc::new(host),
                registry: Arc::new(registry),
                fences: Some(Box::new(move |scope_id: &str| {
                    let path = fence_path.clone();
                    let scope_id = scope_id.to_string();
                    Box::pin(async move {
                        rusqlite::Connection::open(&path)
                            .expect("open the effect journal")
                            .query_row(
                                "SELECT COUNT(*) FROM effect_scope_retirements WHERE scope_id = ?1",
                                [scope_id],
                                |row| row.get(0),
                            )
                            .expect("count fences")
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
            Backend {
                host: Arc::new(storage.effect_host()),
                registry: Arc::new(storage.process_registry()),
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

fn external_registration(process_id: &str) -> lash_core::ProcessRegistration {
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

async fn register_and_complete(registry: &dyn lash_core::ProcessRegistry, process_id: &str) {
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
        .execute_effect(envelope(effect_id), executor())
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
    register_and_complete(backend.registry.as_ref(), process_id).await;
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
            .get_process(process_id)
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
    register_and_complete(backend.registry.as_ref(), process_id).await;
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
