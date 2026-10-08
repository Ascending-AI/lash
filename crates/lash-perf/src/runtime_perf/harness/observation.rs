use std::sync::Arc;

use super::BenchmarkRuntime;
use crate::runtime_perf::providers::BenchmarkToolCatalogObservation;

impl BenchmarkRuntime {
    #[expect(
        clippy::expect_used,
        reason = "the benchmark session is taken by set_up before tool-catalog refresh runs"
    )]
    pub(crate) async fn refresh_tool_catalog(&self, idempotency_key: &str) -> anyhow::Result<()> {
        self.session
            .as_ref()
            .expect("benchmark session")
            .admin()
            .commands()
            .refresh_tool_catalog("runtime perf catalog attribution", idempotency_key)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    }

    #[expect(
        clippy::expect_used,
        reason = "the observer is installed by arm_tool_catalog_observation before suppression can be requested, per the site's message"
    )]
    pub(crate) fn suppress_tool_catalog_composition_counting(&self) {
        self.tool_catalog_observer
            .as_ref()
            .expect("tool-catalog observer")
            .suppress_composition_counting();
    }

    #[expect(
        clippy::expect_used,
        reason = "the observer is installed by arm_tool_catalog_observation before resumption, per the site's message"
    )]
    pub(crate) fn resume_tool_catalog_composition_counting(&self) {
        self.tool_catalog_observer
            .as_ref()
            .expect("tool-catalog observer")
            .resume_composition_counting();
    }

    #[expect(
        clippy::expect_used,
        reason = "the benchmark session and the tool-catalog observer are both installed before arming, per each site's message"
    )]
    pub(crate) fn arm_tool_catalog_observation(
        &self,
        variant: &'static str,
        observation_stage: Arc<dyn Fn() -> u8 + Send + Sync>,
    ) {
        let session_id = self
            .session
            .as_ref()
            .expect("benchmark session")
            .session_id();
        self.tool_catalog_observer
            .as_ref()
            .expect("tool-catalog observer")
            .arm(variant, session_id, observation_stage);
    }

    #[expect(
        clippy::expect_used,
        reason = "finishing returns the observation armed by arm_tool_catalog_observation; taking it again would panic by design, per the message"
    )]
    pub(crate) fn finish_tool_catalog_observation(&self) -> BenchmarkToolCatalogObservation {
        self.tool_catalog_observer
            .as_ref()
            .expect("tool-catalog observer")
            .finish()
    }

    /// The benchmark session's catalog: a catalog is resolved from a
    /// session's recorded facts, never from the core alone.
    pub(crate) async fn tool_catalog_metrics(&self) -> anyhow::Result<(usize, usize)> {
        let manifests = self.session().admin().tools().active_manifests().await?;
        let rendered_bytes = serde_json::to_vec(&manifests)?.len();
        Ok((manifests.len(), rendered_bytes))
    }
}
