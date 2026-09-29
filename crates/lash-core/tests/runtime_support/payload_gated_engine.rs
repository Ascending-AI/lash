use super::*;

/// Engine whose store-free admission function accepts exactly one payload shape.
pub(super) struct PayloadGatedEngine;

pub(super) const PAYLOAD_GATED_ENGINE_KIND: &str = "fig1488-payload-gated";

#[async_trait::async_trait]
impl lash_core::ProcessEngine for PayloadGatedEngine {
    fn kind(&self) -> &'static str {
        PAYLOAD_GATED_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        unreachable!("the payload-gated engine stores no artifacts")
    }

    async fn run(
        &self,
        _context: lash_core::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        Ok(
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                json!({"ran": true}),
            ))
            .into(),
        )
    }
}
