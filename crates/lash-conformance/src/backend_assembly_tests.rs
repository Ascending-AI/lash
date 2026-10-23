//! Laws of durable backend assembly (I0, FIG-5194; ADR 0132 §1): a backend
//! is refused, never defaulted, when its parts disagree.

use std::sync::Arc;

use lash_core::DurableSettings;
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;
use lash_execution_contracts::backend::DurableBuildError;

fn parts() -> BackendParts {
    BackendParts {
        formats: Vec::new(),
        stores: crate::conformance::StoreLawBackend::stores(),
        settings: DurableSettings::default(),
        engines: Vec::new(),
    }
}

fn refusal(parts: BackendParts) -> DurableBuildError {
    match Backend::assemble(parts) {
        Ok(_) => panic!("the backend assembled"),
        Err(error) => error,
    }
}

#[test]
fn two_engines_of_one_kind_are_refused() {
    let refused = refusal(BackendParts {
        engines: vec![
            Arc::new(lash_core::testing::FixtureProcessEngine),
            Arc::new(lash_core::testing::FixtureProcessEngine),
        ],
        ..parts()
    });
    assert!(
        matches!(&refused, DurableBuildError::DuplicateEngine { kind } if kind == "testing-fixture"),
        "{refused:?}"
    );
}

#[test]
fn settings_that_break_a_rule_are_refused() {
    let settings = DurableSettings {
        claim_batch: 0,
        ..DurableSettings::default()
    };
    let refused = refusal(BackendParts {
        settings,
        ..parts()
    });
    assert!(
        matches!(refused, DurableBuildError::InvalidConfig(_)),
        "{refused:?}"
    );
}

struct MismatchedEngine;
#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for MismatchedEngine {
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

    async fn inspect_link(
        &self,
        link: lash_vocabulary::LinkId,
        selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(lash_execution_contracts::process::linking::LinkInspection {
            link,
            document: lash_vocabulary::DocumentId::from_bytes([0; 32]),
            selector: selector.clone(),
            operations: Default::default(),
            machine: Default::default(),
            artifacts: Vec::new(),
            signature: None,
        })
    }
    async fn inspect_start(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Err(
            lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                operation: "generic fixture has no linked code".into(),
            },
        )
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        Default::default()
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
        "testing-fixture"
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
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the fixture engine stores no artifact `{artifact_ref}`"
            )),
        )
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        lash_execution_contracts::process::EngineStateFormat {
            kind: "another-engine".to_owned(),
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
                    lash_core::ToolCallOutput::success(
                        serde_json::json!({ "fixture": "complete" }),
                    ),
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

/// FIG-5235: engines and their declared state formats have one identity.
#[test]
fn an_engine_whose_state_format_names_another_kind_is_refused() {
    let refused = refusal(BackendParts {
        engines: vec![Arc::new(MismatchedEngine)],
        ..parts()
    });
    assert!(
        matches!(refused, DurableBuildError::EngineFormatMismatch { kind, format_kind }
        if kind == "testing-fixture" && format_kind == "another-engine")
    );
}
