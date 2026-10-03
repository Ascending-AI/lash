use lash_core::facade_support::*;
use lash_core::*;
use lash_sansio::core_support::ModelToolReturnCoreSupport as _;
use std::sync::{Arc, Mutex};

const CALL_ID: &str = "oversized-call";
const SEED: u64 = 0x4644_0001;
const LIVE_POLICY: OutputRetentionPolicy = OutputRetentionPolicy {
    inline_limit_bytes: 1024,
    witness_bytes: 256,
};
fn message() -> String {
    "a stack frame of the failed call\n".repeat(600)
}

async fn present_with(
    backend: &Backend,
    scoped: ScopedEffectController<'_>,
    attachments: Arc<RuntimeAttachmentStore>,
    steps: Vec<lash_core::plugin::ToolPresentationStep>,
) -> Result<ModelToolReturn, RuntimeEffectControllerError> {
    let mut factories = lash_core::testing::test_code_protocol_factories();
    let mut spec = lash_core::plugin::PluginSpec::new();
    for step in steps {
        spec = spec.with_presentation_step(lash_core::hook_key!("presentation-step-1"), step);
    }
    factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("retention-law"),
        spec,
    )));
    let context = lash_core::testing::TestExecutionContextBuilder::for_backend(backend)
        .session_id("retention-law-session")
        .plugin_factories(factories)
        .borrowed_effect_controller(scoped)
        .attachment_store(attachments)
        .build()
        .into_runtime();
    lash_core::testing::runtime_internals::complete_tool_output(
        &context,
        ToolCallId::fixture(CALL_ID),
        "oversized",
        ToolCallOutput::success_tool_value(ToolValue::String(message())),
    )
    .await
}

async fn permanent_retention_refusals_settle(engine: impl Into<RetentionEngine>, ended: bool) {
    let engine = engine.into();
    {
        let backend = engine.lash_backend();
        let process = ProcessId::fixture("retention-ended-process");
        if ended {
            backend
                .attachment_referrers()
                .end_attachment_referrer(&ArtifactReferrer::ProcessRecord(process.clone()))
                .await
                .expect("end the referrer");
        }
        let attachments = Arc::new(
            if ended {
                RuntimeAttachmentStore::new(
                    backend.attachment_store(),
                    backend.attachment_referrers(),
                    RuntimeOwner::Process(process),
                )
            } else {
                RuntimeAttachmentStore::ephemeral(backend.attachment_store())
                    .with_max_attachment_bytes(Some(64))
            }
            .with_output_retention(LIVE_POLICY),
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step: lash_core::plugin::ToolPresentationStep = {
            let calls = Arc::clone(&calls);
            Arc::new(move |input| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    // Catching a refused put must not turn it into presentation text.
                    let _ = input.context.artifacts.retain_text("law", &message()).await;
                    Ok(ModelToolReturn::text(
                        input.context.tool_name,
                        "caught refusal",
                    ))
                })
            })
        };
        let observed = Arc::new(Mutex::new(None));
        let results = Arc::clone(&observed);
        let attempt: lash_restate_test::HandlerAttempt = Arc::new(move |scoped| {
            let backend = backend.clone();
            let attachments = Arc::clone(&attachments);
            let step = Arc::clone(&step);
            let calls = Arc::clone(&calls);
            let results = Arc::clone(&results);
            Box::pin(async move {
                let first = present_with(&backend, scoped, attachments, vec![step])
                    .await
                    .expect_err("a permanent put refusal fails presentation");
                *results.lock().expect("results") =
                    Some((first, calls.load(std::sync::atomic::Ordering::SeqCst)));
            })
        });
        engine
            .run_in_handler(
                AdmittedScope::turn(
                    "retention-law-session",
                    if ended { "ended" } else { "size" },
                ),
                attempt,
            )
            .await
            .expect("terminal presentation settles without retry");
        let (first, calls) = observed
            .lock()
            .expect("results")
            .take()
            .expect("presentation ran");
        assert!(
            first.is_terminal(),
            "permanent retention is terminal: {first:?}"
        );
        assert!(
            !first
                .journal_disposition(RuntimeEffectKind::PresentToolResult)
                .is_retryable_derivation()
        );
        assert!(
            first.cause.is_some(),
            "the attachment-store cause stays typed"
        );
        let evidence = serde_json::to_value(&first).expect("typed refusal evidence");
        assert_eq!(evidence["cause"]["kind"], "attachment_retention");
        if ended {
            assert_eq!(
                evidence["cause"]["failure"]["kind"],
                "referrers_operation_failed"
            );
            assert_eq!(
                evidence["cause"]["failure"]["source"]["cause"]["kind"],
                "artifact_referrer_ended"
            );
        } else {
            assert_eq!(evidence["cause"]["failure"]["kind"], "size_limit_exceeded");
            assert_eq!(evidence["cause"]["failure"]["byte_len"], message().len());
            assert_eq!(evidence["cause"]["failure"]["max_bytes"], 64);
        }
        let wire = serde_json::to_vec(&first).expect("encode the typed refusal");
        let decoded: RuntimeEffectControllerError =
            serde_json::from_slice(&wire).expect("decode refusal");
        assert_eq!(decoded.cause, first.cause);
        assert!(first.journaled, "permanent refusal is recorded");
        assert_eq!(calls, 1, "a terminal refusal never retries its put");
    }
}

async fn transient_step_failure_never_becomes_text(engine: impl Into<RetentionEngine>) {
    let engine = engine.into();
    let backend = engine.lash_backend();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let step: lash_core::plugin::ToolPresentationStep = {
        let calls = Arc::clone(&calls);
        Arc::new(move |input| {
            let fail = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            Box::pin(async move {
                if fail {
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RecordedRendererUnavailable,
                        "transient step fault",
                    )
                    .retryable_uncommitted_derivation()
                    .into());
                }
                Ok(ModelToolReturn::text(
                    input.context.tool_name,
                    "recovered presentation",
                ))
            })
        })
    };
    let results = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let step = Arc::clone(&step);
        let results = Arc::clone(&results);
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let step = Arc::clone(&step);
            let results = Arc::clone(&results);
            Box::pin(async move {
                let attachments = Arc::new(RuntimeAttachmentStore::ephemeral(
                    backend.attachment_store(),
                ));
                let first = present_with(&backend, scoped, attachments, vec![step]).await;
                *results.lock().expect("results") = Some(first);
            })
        })
    };
    engine
        .run_in_handler(
            AdmittedScope::turn("retention-law-session", "transient"),
            attempt,
        )
        .await
        .expect("uncommitted presentation recovers");
    let recovered = results
        .lock()
        .expect("results")
        .take()
        .expect("presentation ran");
    let recovered = recovered.expect("redrive reruns the failed step");
    assert_eq!(
        recovered.parts,
        vec![ModelToolReturnPart::text("recovered presentation")],
        "a transient step fault never becomes recorded text",
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "only the failed derivation is retried"
    );
}

enum RetentionEngine {
    Double(lash_restate_test::RestateTestBackend<dyn StoreSet>),
    Live(lash_restate_test::live::LiveRestateBackend<dyn StoreSet>),
}

impl From<lash_restate_test::RestateTestBackend<dyn StoreSet>> for RetentionEngine {
    fn from(double: lash_restate_test::RestateTestBackend<dyn StoreSet>) -> Self {
        Self::Double(double)
    }
}

impl RetentionEngine {
    fn lash_backend(&self) -> Backend {
        match self {
            Self::Double(engine) => engine.lash_backend(),
            Self::Live(engine) => engine.lash_backend(),
        }
    }

    async fn run_in_handler(
        &self,
        scope: AdmittedScope,
        attempt: lash_restate_test::HandlerAttempt,
    ) -> Result<(), String> {
        match self {
            Self::Double(engine) => engine.run_in_handler(scope, attempt).await,
            Self::Live(engine) => {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    engine.run_in_handler(scope, attempt),
                )
                .await
                .map_err(|_| "presentation retried instead of settling".to_string());
                engine.finish().await;
                result?
            }
        }
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the live law requires its private service gate"
)]
fn live_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required by the retention law"))
}

async fn retention_live(postgres: bool, path: &std::path::Path) -> RetentionEngine {
    // Allocate a distinct endpoint and namespace for every real invocation.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve endpoint port");
    let endpoint_bind = listener.local_addr().expect("endpoint address");
    drop(listener);
    let tag = format!(
        "retention4644{}",
        path.file_name()
            .expect("unique temporary directory")
            .to_string_lossy()
            .trim_start_matches('.')
            .to_ascii_lowercase()
    );
    let config = lash_restate_test::live::LiveConfig {
        ingress_url: live_env("RESTATE_INGRESS_URL"),
        admin_url: live_env("RESTATE_ADMIN_URL"),
        endpoint_bind,
        endpoint_url: format!("http://{endpoint_bind}"),
        namespace: tag.parse().expect("retention namespace"),
        run_tag: tag,
    };
    let path = path.to_path_buf();
    let engine = lash_restate_test::live::LiveRestateBackend::start_with_store_set(
        config,
        move |clock| async move {
            let stores: Arc<dyn StoreSet> = if postgres {
                let storage = lash_postgres_store::PostgresStorage::connect(
                    &lash_postgres_store::testing::required_database_url(),
                )
                .await
                .map_err(|error| lash_restate_test::live::LiveError::Stores(error.to_string()))?;
                Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                    &storage,
                    Arc::new(FileAttachmentStore::new(path)),
                    WakeDeliveryConfig::default(),
                    clock,
                ))
            } else {
                Arc::new(
                    lash_sqlite_store::SqliteStoreSet::open_with_clock(&path, clock)
                        .await
                        .map_err(|error| {
                            lash_restate_test::live::LiveError::Stores(error.to_string())
                        })?,
                )
            };
            Ok(stores)
        },
    )
    .await
    .expect("live retention backend");
    RetentionEngine::Live(engine)
}

async fn retention_double(
    postgres: bool,
    file: Option<&std::path::Path>,
) -> lash_restate_test::RestateTestBackend<dyn StoreSet> {
    let path = file.map(std::path::Path::to_path_buf);
    lash_restate_test::backend_with_store_set(
        SEED + 3,
        Default::default(),
        Default::default(),
        move |clock| async move {
            let stores: Arc<dyn StoreSet> =
                if postgres {
                    let storage = lash_postgres_store::PostgresStorage::connect(
                        &lash_postgres_store::testing::required_database_url(),
                    )
                    .await
                    .map_err(|error| lash_restate_test::BackendError::Stores(error.to_string()))?;
                    let bytes = Arc::new(FileAttachmentStore::new(
                        path.expect("PostgreSQL blob directory"),
                    ));
                    Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                        &storage,
                        bytes,
                        WakeDeliveryConfig::default(),
                        clock,
                    ))
                } else if let Some(path) = path {
                    Arc::new(
                        lash_sqlite_store::SqliteStoreSet::open(&path)
                            .await
                            .map_err(|error| {
                                lash_restate_test::BackendError::Stores(error.to_string())
                            })?,
                    )
                } else {
                    Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.map_err(
                        |error| lash_restate_test::BackendError::Stores(error.to_string()),
                    )?)
                };
            Ok(stores)
        },
    )
    .await
    .expect("retention law backend")
}

#[tokio::test]
async fn permanent_retention_refusals_settle_on_sqlite_memory() {
    permanent_retention_refusals_settle(retention_double(false, None).await, false).await;
}
#[tokio::test]
async fn permanent_retention_refusals_settle_on_sqlite_file() {
    let dir = tempfile::tempdir().expect("SQLite file directory");
    permanent_retention_refusals_settle(retention_double(false, Some(dir.path())).await, false)
        .await;
}
#[tokio::test]
#[ignore = "requires PostgreSQL; run inside a pg16 gate"]
async fn permanent_retention_refusals_settle_on_postgres() {
    let dir = tempfile::tempdir().expect("PostgreSQL blob directory");
    permanent_retention_refusals_settle(retention_double(true, Some(dir.path())).await, false)
        .await;
}
#[tokio::test]
async fn transient_step_failure_never_becomes_text_on_sqlite_memory() {
    transient_step_failure_never_becomes_text(retention_double(false, None).await).await;
}
#[tokio::test]
async fn transient_step_failure_never_becomes_text_on_sqlite_file() {
    let dir = tempfile::tempdir().expect("SQLite file directory");
    transient_step_failure_never_becomes_text(retention_double(false, Some(dir.path())).await)
        .await;
}
#[tokio::test]
#[ignore = "requires PostgreSQL; run inside a pg16 gate"]
async fn transient_step_failure_never_becomes_text_on_postgres() {
    let dir = tempfile::tempdir().expect("PostgreSQL blob directory");
    transient_step_failure_never_becomes_text(retention_double(true, Some(dir.path())).await).await;
}

#[tokio::test]
async fn ended_referrer_retention_settles_on_sqlite_memory() {
    permanent_retention_refusals_settle(retention_double(false, None).await, true).await;
}
#[tokio::test]
async fn ended_referrer_retention_settles_on_sqlite_file() {
    let dir = tempfile::tempdir().expect("SQLite file directory");
    permanent_retention_refusals_settle(retention_double(false, Some(dir.path())).await, true)
        .await;
}
#[tokio::test]
#[ignore = "requires PostgreSQL; run inside a pg16 gate"]
async fn ended_referrer_retention_settles_on_postgres() {
    let dir = tempfile::tempdir().expect("PostgreSQL blob directory");
    permanent_retention_refusals_settle(retention_double(true, Some(dir.path())).await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires native Restate; run inside a Restate gate"]
async fn live_permanent_retention_refusals_settle_on_sqlite() {
    let dir = tempfile::tempdir().expect("SQLite live directory");
    permanent_retention_refusals_settle(retention_live(false, dir.path()).await, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires native Restate and PostgreSQL; run inside both gates"]
async fn live_permanent_retention_refusals_settle_on_postgres() {
    let dir = tempfile::tempdir().expect("PostgreSQL live blob directory");
    permanent_retention_refusals_settle(retention_live(true, dir.path()).await, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires native Restate; run inside a Restate gate"]
async fn live_ended_referrer_retention_settles_on_sqlite() {
    let dir = tempfile::tempdir().expect("SQLite live directory");
    permanent_retention_refusals_settle(retention_live(false, dir.path()).await, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires native Restate and PostgreSQL; run inside both gates"]
async fn live_ended_referrer_retention_settles_on_postgres() {
    let dir = tempfile::tempdir().expect("PostgreSQL live blob directory");
    permanent_retention_refusals_settle(retention_live(true, dir.path()).await, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires native Restate; run inside a Restate gate"]
async fn live_transient_step_failure_never_becomes_text_on_sqlite() {
    let dir = tempfile::tempdir().expect("SQLite live directory");
    transient_step_failure_never_becomes_text(retention_live(false, dir.path()).await).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires native Restate and PostgreSQL; run inside both gates"]
async fn live_transient_step_failure_never_becomes_text_on_postgres() {
    let dir = tempfile::tempdir().expect("PostgreSQL live blob directory");
    transient_step_failure_never_becomes_text(retention_live(true, dir.path()).await).await;
}
