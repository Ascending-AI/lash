use super::*;

/// Engine whose store-free admission function accepts exactly one payload shape.
pub(super) struct PayloadGatedEngine;

pub(super) const PAYLOAD_GATED_ENGINE_KIND: &str = "fig1488-payload-gated";

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for PayloadGatedEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash_execution_contracts::process::PerformedEffects, lash_core::ProcessEnvRefusal>
    {
        Ok(lash_execution_contracts::process::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        PAYLOAD_GATED_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_execution_contracts::plugin_error::PluginError>
    {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        unreachable!("the payload-gated engine stores no artifacts")
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        lash_execution_contracts::process::EngineStateFormat {
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

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_execution_contracts::process::EngineState,
        _event: lash_execution_contracts::process::EngineEvent,
    ) -> Result<
        (
            lash_execution_contracts::process::EngineState,
            lash_execution_contracts::process::EngineAction,
        ),
        lash_execution_contracts::process::ProcessInfraError,
    > {
        Ok((
            state,
            lash_execution_contracts::process::EngineAction::Terminal(
                lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(json!({"ran": true})),
                ),
            ),
        ))
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
    }
}
