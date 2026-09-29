//! Every core runs every obligation kind's relay (ADR 0109 §1.4): the store
//! set arms each kind, so a core that skips one leaves its rows owed forever.

use super::*;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use lash_core::StoreError;
use lash_core::store::{
    ArtifactCleanupLedger, ClaimToken, ClaimedObligation, ObligationId, ObligationKey,
    ObligationKind, ObligationLedger, ObligationSettlement, ObligationStanding, SettleOutcome,
    StalledObligation,
};

type Passes = Arc<std::sync::Mutex<BTreeSet<ObligationKind>>>;

/// A ledger that records each due pass a relay runs over it.
struct DuePasses<L: ?Sized + ObligationLedger> {
    inner: Arc<L>,
    passes: Passes,
}

#[async_trait]
impl<L: ?Sized + ObligationLedger> ObligationLedger for DuePasses<L> {
    fn kind(&self) -> ObligationKind {
        self.inner.kind()
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> std::result::Result<Option<ObligationId>, StoreError> {
        self.inner.arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> std::result::Result<Vec<ClaimedObligation>, StoreError> {
        self.passes.lock_recover().insert(self.inner.kind());
        self.inner.claim_due(now_ms, claim_ttl_ms, limit).await
    }

    async fn claim(
        &self,
        id: &ObligationId,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> std::result::Result<Option<ClaimedObligation>, StoreError> {
        self.inner.claim(id, now_ms, claim_ttl_ms).await
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> std::result::Result<SettleOutcome, StoreError> {
        self.inner.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> std::result::Result<bool, StoreError> {
        self.inner.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> std::result::Result<Vec<StalledObligation>, StoreError> {
        self.inner.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> std::result::Result<u64, StoreError> {
        self.inner.count_stalled().await
    }

    async fn standing(
        &self,
        id: &ObligationId,
    ) -> std::result::Result<Option<ObligationStanding>, StoreError> {
        self.inner.standing(id).await
    }
}

#[async_trait]
impl ArtifactCleanupLedger for DuePasses<dyn ArtifactCleanupLedger> {
    async fn arm_cleanup(
        &self,
        cleanup: &lash_core::ArtifactCleanup,
        now_ms: u64,
    ) -> std::result::Result<ObligationId, StoreError> {
        self.inner.arm_cleanup(cleanup, now_ms).await
    }

    async fn nudge(
        &self,
        referrer: &lash_core::ArtifactReferrer,
        now_ms: u64,
    ) -> std::result::Result<bool, StoreError> {
        self.inner.nudge(referrer, now_ms).await
    }

    async fn load_cleanup(
        &self,
        id: &ObligationId,
    ) -> std::result::Result<Option<lash_core::ArtifactCleanup>, StoreError> {
        self.inner.load_cleanup(id).await
    }
}

/// A fresh Restate double backend whose obligation ledgers record the due
/// passes run over them.
async fn recorded_backend() -> (lash_core::Backend, Passes) {
    let passes = Passes::default();
    let recorded = Arc::clone(&passes);
    let cleanup_passes = Arc::clone(&passes);
    let backend = crate::testing::LayeredBackend::over(double_backend().await)
        .map_obligation_ledgers(move |_kind, inner| {
            Arc::new(DuePasses {
                inner,
                passes: Arc::clone(&recorded),
            }) as Arc<dyn ObligationLedger>
        })
        .map_artifact_cleanup(move |inner| {
            Arc::new(DuePasses {
                inner,
                passes: cleanup_passes,
            }) as Arc<dyn ArtifactCleanupLedger>
        })
        .into_backend();
    (backend, passes)
}

fn configured(builder: crate::core::LashCoreBuilder) -> crate::core::LashCoreBuilder {
    builder
        .model(mock_model_spec())
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
}

/// The kinds `passes` has not seen a due pass for once `core` has run
/// long enough to take the recovery lease and tick.
async fn unrelayed(core: LashCore, passes: Passes) -> Vec<ObligationKind> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let missing: Vec<ObligationKind> = ObligationKind::ALL
            .into_iter()
            .filter(|kind| !passes.lock_recover().contains(kind))
            .collect();
        if missing.is_empty() || std::time::Instant::now() >= deadline {
            drop(core);
            return missing;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// A core built through each public builder path runs a due pass of every
/// obligation kind the store set arms — none is left to a host to wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_builder_path_runs_every_obligation_kinds_relay() {
    let owner = crate::testing::runtime_lease_owner;
    let mut paths: Vec<(&str, LashCore, Passes)> = Vec::new();

    let (backend, passes) = recorded_backend().await;
    let core = configured(
        LashCore::builder(backend, crate::TurnBudget::Unbounded).protocol_plugin(Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory::new(),
        )),
    )
    .build(owner())
    .expect("build through the builder");
    paths.push(("builder", core, passes));

    let (backend, passes) = recorded_backend().await;
    let core = configured(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .build(owner())
    .expect("build through the standard builder");
    paths.push(("standard_builder", core, passes));

    let (backend, passes) = recorded_backend().await;
    let core = configured(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .advanced()
    .build(owner())
    .expect("build through the advanced builder");
    paths.push(("advanced", core, passes));

    #[cfg(feature = "rlm")]
    {
        let (backend, passes) = recorded_backend().await;
        let config: crate::rlm::RlmProtocolPluginConfig =
            serde_json::from_value(serde_json::json!({
                "channel": "cell",
                "instruction_limit": { "bounded": 1_000_000 },
                "memory_limit": { "bounded": 67_108_864 },
            }))
            .expect("rlm config");
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(config, &backend);
        let core = configured(LashCore::rlm_builder(
            backend,
            crate::TurnBudget::Unbounded,
            factory,
        ))
        .build(owner())
        .expect("build through the rlm builder");
        paths.push(("rlm_builder", core, passes));
    }

    let names: Vec<&str> = paths.iter().map(|(name, _, _)| *name).collect();
    let missing = futures_util::future::join_all(
        paths
            .into_iter()
            .map(|(_, core, passes)| unrelayed(core, passes)),
    )
    .await;
    let unrelayed: Vec<(&str, Vec<ObligationKind>)> = names
        .into_iter()
        .zip(missing)
        .filter(|(_, missing)| !missing.is_empty())
        .collect();
    assert!(
        unrelayed.is_empty(),
        "a core ran no relay for these obligation kinds: {unrelayed:?}"
    );
}
