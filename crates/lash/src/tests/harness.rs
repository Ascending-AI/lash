use super::*;

pub(crate) fn llm_profile_spec(
    model: impl Into<String>,
    variant: Option<String>,
    context_window_tokens: usize,
) -> lash_core::LlmProfileMetadata {
    let capability = capability_for_variant(variant.as_deref());
    lash_core::LlmProfileMetadata::builder(model)
        .context_window_tokens(context_window_tokens)
        .build()
        .expect("valid model spec")
        .with_capability(capability)
}

pub(crate) fn mock_llm_profile_spec() -> lash_core::LlmProfileMetadata {
    llm_profile_spec("mock-model", None, 200_000)
}

/// The spec these laws create a session from when they state nothing of
/// their own: the test host's own default value, the mock model with an
/// unbounded turn budget. A core keeps none (FIG-4594).
pub(crate) fn mock_session_spec() -> crate::SessionSpec {
    session_spec_for(&mock_llm_profile_spec())
}

/// An unbounded spec running `metadata`, by the key `serve_test_llm_profile` and
/// [`test_catalog`] register it under: its wire model.
pub(crate) fn session_spec_for(metadata: &lash_core::LlmProfileMetadata) -> crate::SessionSpec {
    crate::SessionSpec::new(
        metadata.wire_model.clone(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    )
}

/// `metadata` as [`test_catalog`] records it, run with the provider's default
/// reasoning.
pub(crate) fn recorded_llm_profile(
    metadata: lash_core::LlmProfileMetadata,
) -> lash_core::LlmProfileConfig {
    lash_core::testing::test_llm_profile_config(metadata.wire_model.clone(), metadata)
}

/// A fresh SQLite memory store set: storage ports only, no engine. For a
/// test whose every use is a store port.
pub(crate) async fn sqlite_memory_store_set() -> Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    stores
}

/// A PostgreSQL store set over an isolated database of the service the
/// gate hands the run, and what must outlive it: the database and the
/// attachment directory. For a law's `#[ignore]`d PostgreSQL leg.
pub(crate) async fn postgres_store_parts() -> (
    Arc<lash_postgres_store::PostgresStoreSet>,
    lash_postgres_store::testing::IsolatedDatabase,
    tempfile::TempDir,
) {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("connect PostgreSQL");
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        lash_sqlite_store::SqliteStoreSet::open((attachments.path()).join("attachments.db"))
            .await
            .expect("SQLite attachment store")
            .attachment_store(),
    ));
    (stores, database, attachments)
}

/// A backend over a fresh SQLite memory store set whose effect host only
/// records: for a test that needs a backend value but runs no effect.
pub(crate) async fn sqlite_memory_store_backend() -> lash_core::Backend {
    let stores = sqlite_memory_store_set().await;
    lash_conformance::backend_over(stores)
}

/// A backend over a fresh SQLite memory store set stamping from `clock`,
/// whose effect host only records and which executes no session work: for a
/// test that reads what the stores stamp.
pub(crate) async fn store_backend_with_clock(
    clock: Arc<dyn lash_core::Clock>,
) -> lash_core::Backend {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
            .await
            .expect("open a SQLite memory store set"),
    );
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    lash_conformance::backend_over(stores)
}

/// A fresh PostgreSQL store set on an isolated database of the gate's
/// server, and what must outlive it.
#[allow(
    clippy::disallowed_methods,
    reason = "the test host reads the PostgreSQL service URL its gate sets"
)]
pub(crate) async fn postgres_store_set() -> (Arc<dyn lash_core::StoreSet>, Box<dyn std::any::Any>) {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("connect PostgreSQL");
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        lash_sqlite_store::SqliteStoreSet::open((attachments.path()).join("attachments.db"))
            .await
            .expect("SQLite attachment store")
            .attachment_store(),
    )) as Arc<dyn lash_core::StoreSet>;
    (stores, Box::new((database, attachments, storage)))
}

/// One backend with some of its ports decorated by a test that observes
/// or faults them. Every port a test does not decorate is the inner
/// backend's, and every decoration is handed the inner port it wraps, so
/// the decorated backend is still one substrate.
#[derive(Clone)]
pub(crate) struct DecoratedBackend {
    layered: lash_core::testing::runtime_helpers::LayeredBackend,
}

impl DecoratedBackend {
    pub(crate) fn over(inner: lash_core::Backend) -> Self {
        Self {
            layered: lash_core::testing::runtime_helpers::LayeredBackend::over(inner),
        }
    }

    pub(crate) fn process_env_store(
        self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::ProcessExecutionEnvStore>,
        ) -> Arc<dyn lash_core::ProcessExecutionEnvStore>,
    ) -> Self {
        Self {
            layered: self.layered.map_process_env_store(decorate),
        }
    }

    pub(crate) fn session_store_factory(
        self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::DeploymentStore>,
        ) -> Arc<dyn lash_core::DeploymentStore>,
    ) -> Self {
        Self {
            layered: self.layered.map_session_store_factory(decorate),
        }
    }

    /// The decorated backend.
    pub(crate) fn into_backend(self) -> lash_core::Backend {
        self.layered.into_backend()
    }
}

impl From<DecoratedBackend> for lash_core::Backend {
    fn from(decorated: DecoratedBackend) -> Self {
        decorated.into_backend()
    }
}

/// The runtime settings every facade test core names: a generous commit
/// budget and single-row queued-work batching.
pub(crate) fn explicit_ephemeral_facets(
    builder: crate::core::LashCoreBuilder,
) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets_with_budget(builder, crate::CommitBudget::bounded(1024 * 1024, 512))
}

/// [`explicit_ephemeral_facets`] with an explicit commit budget.
pub(crate) fn explicit_ephemeral_facets_with_budget(
    builder: crate::core::LashCoreBuilder,
    commit_budget: crate::CommitBudget,
) -> crate::core::LashCoreBuilder {
    builder
        .commit_budget(commit_budget)
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
}

fn capability_for_variant(variant: Option<&str>) -> lash_core::LlmProfileCapability {
    let Some(variant) = variant else {
        return lash_core::LlmProfileCapability::default();
    };
    lash_core::LlmProfileCapability {
        instruction_role: Default::default(),
        native_mid_conversation_system: false,
        google_dialect: Default::default(),
        reasoning: Some(lash_core::ReasoningCapability {
            efforts: vec![variant.to_string()],
            encoding: lash_core::ReasoningEncoding::Effort,
            disable: false,
            mandatory: false,
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
        reasoning_retention: Default::default(),
    }
}
