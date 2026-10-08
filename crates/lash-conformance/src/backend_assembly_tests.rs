//! Laws of durable backend assembly (I0, FIG-5194; ADR 0132 §1): a backend
//! is refused, never defaulted, when its parts disagree.

use std::any::Any;
use std::sync::Arc;

use lash_core::{
    Backend, BackendParts, DurableBuildError, DurableSettings, NoProjectionProviders,
    ProjectionProviders,
};

fn parts() -> BackendParts {
    BackendParts {
        formats: Vec::new(),
        stores: crate::conformance::StoreLawBackend::stores(),
        settings: DurableSettings::default(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
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

struct Projections(Vec<&'static str>);

impl ProjectionProviders for Projections {
    fn projection_types(&self) -> Vec<String> {
        self.0.iter().map(|kind| (*kind).to_owned()).collect()
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

#[test]
fn two_providers_of_one_projection_type_are_refused() {
    let refused = refusal(BackendParts {
        providers: Arc::new(Projections(vec!["page", "issue", "page"])),
        ..parts()
    });
    assert!(
        matches!(&refused, DurableBuildError::DuplicateProvider { projection } if projection == "page"),
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
impl lash_core::ProcessEngine for MismatchedEngine {
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
        "testing-fixture"
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
        artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Session(format!(
            "the fixture engine stores no artifact `{artifact_ref}`"
        )))
    }

    fn state_format(&self) -> lash_core::EngineStateFormat {
        lash_core::EngineStateFormat {
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
                lash_core::ToolCallOutput::success(serde_json::json!({ "fixture": "complete" })),
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
