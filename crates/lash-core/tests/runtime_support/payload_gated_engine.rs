use super::*;

/// Engine whose store-free admission function accepts exactly one payload shape.
pub(super) struct PayloadGatedEngine;

pub(super) const PAYLOAD_GATED_ENGINE_KIND: &str = "fig1488-payload-gated";

#[async_trait::async_trait]
impl lash_core::ProcessEngine for PayloadGatedEngine {
    async fn check_args(
        &self,
        _signature: &lash_core::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_core::ArgsMode,
    ) -> std::result::Result<(), lash_core::ArgsMismatch> {
        Err(lash_core::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

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

    fn state_format(&self) -> lash_core::EngineStateFormat {
        lash_core::EngineStateFormat {
            kind: self.kind().to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, lash_core::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_core::EngineState,
        _event: lash_core::EngineEvent,
    ) -> Result<(lash_core::EngineState, lash_core::EngineAction), lash_core::ProcessInfraError>
    {
        Ok((
            state,
            lash_core::EngineAction::Terminal(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(json!({"ran": true})),
            )),
        ))
    }

    async fn resolve(
        &self,
        _reference: &lash_core::ProcessDefinitionRef,
    ) -> Result<lash_core::ProcessDefinitionResolution, lash_core::ProcessDefinitionRefusal> {
        Ok(lash_core::ProcessDefinitionResolution::new(
            lash_core::ProcessSignature::Unknown,
        ))
    }
}
