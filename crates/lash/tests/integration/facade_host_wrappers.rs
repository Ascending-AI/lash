//! A host that depends on the facade alone wraps what the runtime hands it
//! (FIG-4373), and a bounded live-replay store recovers an evicted cursor
//! through the facade (FIG-4295).
//!
//! Two host shapes the figments upgrade needs, written with `lash::` paths and
//! nothing else (the store tiers under them are test fixture, not host code):
//!
//! * a [`PluginFactory`](lash::plugins::PluginFactory) that wraps the RLM
//!   protocol factory and forwards its process-engine contributions, which
//!   needs `ProcessEngineContributionContext` to be nameable; without the
//!   forward the Lash VM process engine is dropped;
//! * a [`StoreSet`](lash::StoreSet) decorator that overrides
//!   `definition_store`, which needs every trait object the store set hands
//!   out to be nameable.
//!
//! The laws run real turns on the core's node over SQLite memory, SQLite
//! file and PostgreSQL (FIG-5307 re-wrote them on the durable substrate).

#![cfg(all(feature = "rlm", feature = "sqlite", feature = "testing"))]
#![expect(
    clippy::expect_used,
    reason = "test target: the setup helpers around the law are test code too"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;
use lash::sync::MutexExt;
use lash::{LashCore, TurnInput};

// ---- the wrapping protocol factory ------------------------------------------

/// The host's protocol factory: the RLM recipe, wrapped. Its engine
/// decorator records how many declared engines the recipe contributes.
struct HostRlmFactory {
    inner: Arc<lash::rlm::RlmProtocolPluginFactory>,
    contributed_engines: Arc<Mutex<Vec<usize>>>,
}

#[async_trait::async_trait]
impl lash::plugins::PluginFactory for HostRlmFactory {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    async fn shutdown(&self) -> Result<(), lash::plugins::PluginError> {
        self.inner.shutdown().await
    }

    fn bound_backend(&self) -> Option<&str> {
        self.inner.bound_backend()
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let recipe_factory = Arc::clone(&self);
        let mut recipe = Arc::clone(&self.inner).recipe()?;
        let witness = Arc::clone(&recipe_factory.contributed_engines);
        recipe = recipe.decorate_process_engines(move |engine| {
            witness.lock_recover().push(1);
            Ok(engine)
        });
        Ok(recipe)
    }
}

impl HostRlmFactory {
    pub fn declaration() -> lash::plugins::PluginDeclaration {
        lash::rlm::RlmProtocolPluginFactory::declaration()
    }
}
impl lash::plugins::PluginMetadata for HostRlmFactory {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        Self::declaration()
    }
}

// ---- the decorating store set -----------------------------------------------

/// The host's store set: the tier's, with `definition_store` counted.
/// Every other port is the inner store set's.
struct HostStores {
    inner: Arc<dyn lash::StoreSet>,
    definition_reads: Arc<AtomicUsize>,
}

impl lash::StoreSet for HostStores {
    fn package_store(&self) -> Arc<dyn lash::durable::packages::PackageStore> {
        self.inner.package_store()
    }
    fn execution_resolutions(&self) -> Arc<dyn lash::durable::resolutions::ExecutionResolutions> {
        self.inner.execution_resolutions()
    }

    fn durable_store(&self) -> Arc<dyn lash::durable::DurableStore> {
        self.inner.durable_store()
    }

    fn wake_sink(&self) -> lash::durable::runner::WakeSink {
        self.inner.wake_sink()
    }

    fn node_wakes(&self) -> Option<Arc<dyn lash::durable::NodeWakes>> {
        self.inner.node_wakes()
    }

    fn binding_identity(&self) -> &lash::StoreBindingId {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash::runtime::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn lash::persistence::DeploymentStore> {
        self.inner.session_store_factory()
    }

    fn attachment_referrers(&self) -> Arc<dyn lash::persistence::AttachmentReferrers> {
        self.inner.attachment_referrers()
    }

    fn process_registry(&self) -> Arc<dyn lash::persistence::ProcessRegistry> {
        self.inner.process_registry()
    }

    fn process_authority_reads(
        &self,
    ) -> Arc<dyn lash_execution_contracts::backend::ProcessAuthorityReads> {
        self.inner.process_authority_reads()
    }

    fn process_env_store(&self) -> Arc<dyn lash::persistence::ProcessEnvStore> {
        self.inner.process_env_store()
    }

    fn turn_prelude_store(&self) -> Arc<dyn lash::persistence::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }

    fn tool_material_store(&self) -> Arc<dyn lash::persistence::ToolMaterialStore> {
        self.inner.tool_material_store()
    }

    fn definition_store(&self) -> Arc<dyn lash::persistence::ProcessDefinitionStore> {
        self.definition_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.definition_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash::persistence::AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn lash::persistence::ModuleArtifactStore> {
        self.inner.module_artifacts()
    }

    fn obligation_ledger(
        &self,
        kind: lash::ObligationKind,
    ) -> Arc<dyn lash::persistence::ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }

    fn artifact_cleanup(&self) -> Arc<dyn lash::persistence::ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }
}

// ---- the tiers --------------------------------------------------------------

/// The database a law runs over.
#[derive(Clone, Copy)]
enum Tier {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// The durable backend over the tier's store set, decorated by `decorate`
/// before the backend is built, so every store read the core and its node
/// make goes through the decorator; with what the database must outlive.
async fn backend(
    tier: Tier,
    decorate: impl FnOnce(Arc<dyn lash::StoreSet>) -> Arc<dyn lash::StoreSet>,
) -> (lash::Backend, Vec<Box<dyn std::any::Any + Send>>) {
    let (stores, keep): (Arc<dyn lash::StoreSet>, Vec<Box<dyn std::any::Any + Send>>) = match tier {
        Tier::SqliteMemory => (
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("SQLite memory stores"),
            ),
            Vec::new(),
        ),
        Tier::SqliteFile => {
            let root = tempfile::tempdir().expect("SQLite store directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open(
                root.path().join("lash.db"),
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
            .await
            .expect("SQLite file stores");
            (Arc::new(stores), vec![Box::new(root)])
        }
        Tier::Postgres => {
            let url = lash_postgres_store::testing::required_database_url();
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("attachment directory");
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                lash_sqlite_store::SqliteStoreSet::open(
                    (attachments.path()).join("attachments.db"),
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
                .await
                .expect("SQLite attachment store")
                .attachment_store(),
            );
            (
                Arc::new(stores),
                vec![Box::new(database), Box::new(storage), Box::new(attachments)],
            )
        }
    };
    let backend = lash::durable::DurableBackendBuilder::new(decorate(stores))
        .build()
        .expect("the durable backend builds");
    (backend, keep)
}

// ---- the host ---------------------------------------------------------------

fn response(code: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: format!("<typescript>\n{code}\n</typescript>"),
            response_meta: None,
        }],
        ..Default::default()
    }
}

fn core(
    backend: lash::Backend,
    replies: Vec<LlmResponse>,
    contributed_engines: &Arc<Mutex<Vec<usize>>>,
) -> LashCore {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let provider = lash::testing::TestProvider::builder()
        .kind("facade-host-wrappers")
        .complete(move |_request| {
            let replies = Arc::clone(&replies);
            async move {
                Ok(replies
                    .lock_recover()
                    .pop_front()
                    .expect("scripted reply queue is exhausted"))
            }
        })
        .build()
        .into_handle();
    let protocol = HostRlmFactory {
        inner: Arc::new(lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            lash::rlm::CellDialect::typescript(),
        )),
        contributed_engines: Arc::clone(contributed_engines),
    };
    LashCore::builder(backend)
        .protocol_plugin(Arc::new(protocol))
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("facade-host-wrappers")
                .cache_retention(lash::provider::CacheRetention::Short)
                .context_window_tokens(64_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("facade-host-wrappers-worker"),
            lash::persistence::LeaseIncarnationId::new("facade-host-wrappers-boot"),
        ))
        .expect("RLM core behind the host's wrapper")
}

// ---- the law ----------------------------------------------------------------

/// The law: a facade-only host wraps the RLM factory and the store set, the
/// wrapped factory still contributes the Lash VM process engine, a code-mode
/// turn runs through both, and the runtime reads process definitions through
/// the decorator.
async fn a_facade_host_wraps_the_rlm_factory_and_its_stores(tier: Tier) {
    let definition_reads = Arc::new(AtomicUsize::new(0));
    let decorator_reads = Arc::clone(&definition_reads);
    let (backend, _keep) = backend(tier, move |inner| {
        Arc::new(HostStores {
            inner,
            definition_reads: decorator_reads,
        })
    })
    .await;
    let contributed_engines = Arc::new(Mutex::new(Vec::new()));
    let core = core(
        backend.clone(),
        vec![response("await control.finish('wrapped');")],
        &contributed_engines,
    );
    // The core installs a protocol's engines wherever it builds a runtime
    // host; every install goes through the wrapper.
    let forwarded = contributed_engines.lock_recover().clone();
    assert!(
        !forwarded.is_empty() && forwarded.iter().all(|engines| *engines == 1),
        "every forward hands on the RLM factory's one contribution, the Lash VM process \
         engine: {forwarded:?}"
    );

    match core
        .session(lash::SessionId::parse("facade-host-wrappers").expect("nonblank host identity"))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "facade-host-wrappers",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        )))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => panic!("create the session: {error:?}"),
    }
    let session = core
        .session(lash::SessionId::parse("facade-host-wrappers").expect("nonblank host identity"))
        .open()
        .await
        .expect("open the session");
    let output = session
        .send(TurnInput::text("finish through the wrapper"))
        .output()
        .await
        .expect("the turn runs");
    assert!(output.is_success(), "{output:?}");

    let reads_before = definition_reads.load(Ordering::SeqCst);
    let definitions: Arc<dyn lash::persistence::ProcessDefinitionStore> =
        backend.definition_store();
    assert_eq!(
        definition_reads.load(Ordering::SeqCst),
        reads_before + 1,
        "the backend hands out the decorator's process-definition store"
    );
    drop(definitions);
    core.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_facade_host_wraps_the_rlm_factory_and_its_stores_on_sqlite_memory() {
    a_facade_host_wraps_the_rlm_factory_and_its_stores(Tier::SqliteMemory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_facade_host_wraps_the_rlm_factory_and_its_stores_on_sqlite_file() {
    a_facade_host_wraps_the_rlm_factory_and_its_stores(Tier::SqliteFile).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_facade_host_wraps_the_rlm_factory_and_its_stores_on_postgres() {
    a_facade_host_wraps_the_rlm_factory_and_its_stores(Tier::Postgres).await;
}

// ---- live replay bounds -----------------------------------------------------

use lash::observe::{
    InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, LiveReplayGapReason, LiveReplayStore,
    SessionResume,
};

/// A core whose live-replay store holds one session: a turn's events replay
/// from a cursor taken before it, and once another session evicts it, the
/// same cursor recovers through a typed `Unavailable` gap to the committed
/// head.
async fn eviction_law(backend: lash::Backend, tag: &str) -> LashCore {
    let replay = Arc::new(InMemoryLiveReplayStore::new(
        InMemoryLiveReplayStoreConfig {
            max_sessions: 1,
            ..InMemoryLiveReplayStoreConfig::standard()
        },
    ));
    let provider = lash::testing::TestProvider::builder()
        .kind("live-replay-bounds")
        .complete(|_| async {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "retained turn".into(),
                    response_meta: None,
                }],
                ..Default::default()
            })
        })
        .build()
        .into_handle();
    let core = LashCore::standard_builder(backend)
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "live-replay-bounds",
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder("live-replay-bounds")
                            .cache_retention(lash::provider::CacheRetention::Short)
                            .context_window_tokens(64_000)
                            .build()
                            .expect("model"),
                        provider,
                    ),
                )
                .expect("one key registers"),
        ))
        .live_replay_store(replay.clone())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("replay-bounds"),
            lash::persistence::LeaseIncarnationId::new(tag),
        ))
        .expect("core");
    let id = format!("replay-bounds-{tag}");
    core.session(lash::SessionId::fixture(&id))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "live-replay-bounds",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        )))
        .await
        .expect("create");
    let session = core
        .session(lash::SessionId::fixture(&id))
        .open()
        .await
        .expect("open");
    let old = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let output = session
        .send(TurnInput::text("publish before eviction"))
        .output()
        .await
        .expect("turn");
    assert!(output.is_success(), "{output:?}");
    let committed = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");
    assert!(
        matches!(session.observe().resume_from_cursor(&old).await.expect("retained replay"), SessionResume::Replayed { events } if !events.is_empty())
    );

    replay.current_cursor(
        &lash::SessionId::fixture(format!("pressure-{tag}")),
        lash::observe::SessionRevision::new(0),
    );
    let SessionResume::Gap { observation, gap } = session
        .observe()
        .resume_from_cursor(&old)
        .await
        .expect("eviction recovery")
    else {
        panic!("evicted cursor must recover through a gap");
    };
    assert_eq!(gap.reason, LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_eq!(
        observation.read_view.turn_index(),
        committed.read_view.turn_index()
    );
    assert_eq!(
        observation.read_view.messages().len(),
        committed.read_view.messages().len()
    );
    assert_ne!(observation.cursor, old);
    assert!(matches!(
        replay.replay_after_cursor(&old).await,
        Ok(lash::persistence::LiveReplayOutcome::Gap(
            LiveReplayGapReason::Unavailable
        ))
    ));
    assert!(matches!(
        replay.subscribe_after_cursor(&old).await,
        Ok(lash::observe::LiveReplaySubscribeOutcome::Gap(
            LiveReplayGapReason::Unavailable
        ))
    ));
    core
}

/// The eviction law on `tier`.
async fn eviction_recovers_the_facade_on(tier: Tier, tag: &str) {
    let (backend, _keep) = backend(tier, |stores| stores).await;
    eviction_law(backend, tag)
        .await
        .shutdown()
        .await
        .expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_recovers_the_facade_on_sqlite_memory() {
    eviction_recovers_the_facade_on(Tier::SqliteMemory, "sqlite-memory").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_recovers_the_facade_on_sqlite_file() {
    eviction_recovers_the_facade_on(Tier::SqliteFile, "sqlite-file").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn eviction_recovers_the_facade_on_postgres() {
    eviction_recovers_the_facade_on(Tier::Postgres, "postgres").await;
}

/// FIG-6387: hosts opt a nested tool value into durable encoding through
/// the facade, preserving the exact bytes of the durable wrapper.
#[test]
fn a_host_tool_value_field_uses_the_facade_durable_adapter() {
    #[derive(serde::Serialize)]
    struct HostResponse {
        #[serde(serialize_with = "lash::persistence::serialize_durable")]
        value: lash::tools::ToolValue,
    }

    #[derive(serde::Serialize)]
    struct DurableResponse<'a> {
        value: lash::persistence::Durable<&'a lash::tools::ToolValue>,
    }

    let response = HostResponse {
        value: lash::tools::ToolValue::untrusted_json(serde_json::json!({
            "$lash_tool_value": { "kind": "host_data" },
            "answer": 42,
        })),
    };
    let encoded = serde_json::to_vec(&response).expect("encode host response");
    let expected = serde_json::to_vec(&DurableResponse {
        value: lash::persistence::Durable(&response.value),
    })
    .expect("encode durable response");
    assert_eq!(encoded, expected);
}
