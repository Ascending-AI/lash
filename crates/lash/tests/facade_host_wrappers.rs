//! A host that depends on the facade alone wraps what the runtime hands it
//! (FIG-4373).
//!
//! Two host shapes the figments upgrade needs, written with `lash::` paths and
//! nothing else (the store tiers under them come from `tiers.rs`, which is
//! test fixture, not host code):
//!
//! * a [`PluginFactory`](lash::plugins::PluginFactory) that wraps the RLM
//!   protocol factory and forwards its process-engine contributions, which
//!   needs `ProcessEngineContributionContext` to be nameable; without the
//!   forward the Lashlang process engine is dropped;
//! * a [`StoreSet`](lash::StoreSet) decorator that overrides
//!   `definition_store`, which needs every trait object the store set hands
//!   out to be nameable.
//!
//! The law runs a real code-mode turn through both on SQLite memory, SQLite
//! file and PostgreSQL, each under the Restate server double.

#![cfg(all(
    feature = "rlm",
    feature = "restate",
    feature = "sqlite",
    feature = "testing"
))]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    reason = "test target: the host fixtures around the tests are test code too"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;
use lash::sync::MutexExt;
use lash::{LashCore, TurnInput};

#[path = "facade_host_wrappers/tiers.rs"]
mod tiers;
use tiers::Tier;

// ---- the wrapping protocol factory ------------------------------------------

/// The host's protocol factory: the RLM factory, wrapped. Every method
/// forwards; `process_engine_contributions` also counts what it forwards.
struct HostRlmFactory {
    inner: lash::rlm::RlmProtocolPluginFactory,
    contributed_engines: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash::plugins::PluginFactory for HostRlmFactory {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn declaration(&self) -> lash::plugins::PluginDeclaration {
        self.inner.declaration()
    }

    async fn shutdown(&self) -> Result<(), lash::plugins::PluginError> {
        self.inner.shutdown().await
    }

    fn extension_contributions(&self) -> Vec<lash::plugins::PluginExtensionContribution> {
        self.inner.extension_contributions()
    }

    fn bound_backend(&self) -> Option<&str> {
        self.inner.bound_backend()
    }

    fn process_engine_contributions(
        &self,
        ctx: &lash::plugins::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash::plugins::ProcessEngineRegistration>, lash::plugins::PluginError> {
        let engines = self.inner.process_engine_contributions(ctx)?;
        self.contributed_engines
            .fetch_add(engines.len(), Ordering::SeqCst);
        Ok(engines)
    }

    fn build(
        &self,
        ctx: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        self.inner.build(ctx)
    }
}

// ---- the decorating store set -----------------------------------------------

/// The host's store set: the tier's, with `definition_store` counted.
/// Every other half is the inner store set's.
struct HostStores {
    inner: Arc<dyn lash::StoreSet>,
    definition_reads: Arc<AtomicUsize>,
}

impl lash::StoreSet for HostStores {
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

    fn process_registry(&self) -> Arc<dyn lash::process::ProcessRegistry> {
        self.inner.process_registry()
    }

    fn process_continuations(&self) -> Arc<dyn lash::process::ProcessContinuationStore> {
        self.inner.process_continuations()
    }

    fn trigger_store(&self) -> Arc<dyn lash::triggers::TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_env_store(&self) -> Arc<dyn lash::persistence::ProcessExecutionEnvStore> {
        self.inner.process_env_store()
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

    fn recovery_leader(&self) -> Arc<dyn lash::persistence::RecoveryLeaderStore> {
        self.inner.recovery_leader()
    }

    fn generation_drain(&self) -> Arc<dyn lash::persistence::GenerationDrainStore> {
        self.inner.generation_drain()
    }

    fn obligation_ledger(
        &self,
        kind: lash::ObligationKind,
    ) -> Arc<dyn lash::persistence::ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }

    fn session_delete_ledger(&self) -> Arc<dyn lash::persistence::SessionDeleteLedger> {
        self.inner.session_delete_ledger()
    }

    fn artifact_cleanup(&self) -> Arc<dyn lash::persistence::ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }

    fn worker_recovery(&self) -> Arc<dyn lash::persistence::WorkerRecoveryStore> {
        self.inner.worker_recovery()
    }

    fn usage_accounting(&self) -> Arc<dyn lash::persistence::UsageAccountingStore> {
        self.inner.usage_accounting()
    }
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
    contributed_engines: &Arc<AtomicUsize>,
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
        inner: lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash::rlm::TypescriptDialect),
            &backend,
        ),
        contributed_engines: Arc::clone(contributed_engines),
    };
    LashCore::builder(backend)
        .protocol_plugin(Arc::new(protocol))
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("facade-host-wrappers")
                .context_window_tokens(64_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "facade-host-wrappers-worker",
            "facade-host-wrappers-boot",
        ))
        .expect("RLM core behind the host's wrapper")
}

// ---- the law ----------------------------------------------------------------

/// The law: a facade-only host wraps the RLM factory and the store set, the
/// wrapped factory still contributes the Lashlang process engine, a code-mode
/// turn runs through both, and the runtime reads process definitions through
/// the decorator.
async fn a_facade_host_wraps_the_rlm_factory_and_its_stores(tier: Tier, seed: u64) {
    let definition_reads = Arc::new(AtomicUsize::new(0));
    let decorator_reads = Arc::clone(&definition_reads);
    let Some(double) = tiers::double(tier, seed, move |inner| {
        Arc::new(HostStores {
            inner,
            definition_reads: decorator_reads,
        })
    })
    .await
    else {
        return;
    };
    let backend = double.double.lash_backend();
    let contributed_engines = Arc::new(AtomicUsize::new(0));
    let core = core(
        backend.clone(),
        vec![response("finish('wrapped');")],
        &contributed_engines,
    );
    assert_eq!(
        contributed_engines.load(Ordering::SeqCst),
        1,
        "the wrapper forwards the RLM factory's one contribution, the Lashlang process engine"
    );

    match core
        .session("facade-host-wrappers")
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "facade-host-wrappers",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => panic!("create the session: {error:?}"),
    }
    let session = core
        .session("facade-host-wrappers")
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
}

#[tokio::test]
async fn a_facade_host_wraps_the_rlm_factory_and_its_stores_on_sqlite_memory() {
    a_facade_host_wraps_the_rlm_factory_and_its_stores(Tier::SqliteMemory, 0x4373_0001).await;
}

#[tokio::test]
async fn a_facade_host_wraps_the_rlm_factory_and_its_stores_on_sqlite_file() {
    a_facade_host_wraps_the_rlm_factory_and_its_stores(Tier::SqliteFile, 0x4373_0002).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_facade_host_wraps_the_rlm_factory_and_its_stores_on_postgres() {
    a_facade_host_wraps_the_rlm_factory_and_its_stores(Tier::Postgres, 0x4373_0003).await;
}

#[path = "facade_host_wrappers/live_replay_bounds.rs"]
mod live_replay_bounds;
