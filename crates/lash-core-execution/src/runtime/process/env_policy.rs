//! The host's model start policy (D8, D9, D13; final-spec §3.6).
//!
//! A model's `processes.start` names a definition and its arguments; the
//! host's required [`ProcessEnvPolicy`] states the environment and engine
//! settings the child runs under. The policy sees the target's
//! authoritative signature, the starting turn's recorded run, the surface
//! the declaring attempt was admitted under, the grants the declaring
//! cell's committed deferred matches recorded and, inside a process, the
//! enclosing process's environment. Its choice is sealed into the declaration's
//! launch and journaled with the tool outcome: once that outcome commits,
//! realization, redrive, takeover and adoption never call it again.

use std::collections::BTreeMap;

use lash_core_store::process_env::{ProcessEnv, ProcessEnvRefusal};
use lash_core_store::run_spec::ResolvedRun;

use super::ProcessDefinitionId;
use super::ProcessEngineRegistry;
use super::model::ProcessEnvRef;
use crate::{ExecutionOwner, SurfaceTools};
use lash_execution_contracts::process::EngineSettings;
use lash_execution_contracts::process::ProcessSignature;
use lash_vocabulary::identity::ProcessId;

/// Chooses the environment and engine settings of a model's process start.
///
/// Async because real host mappings read databases and registries; a sync
/// function would force a blocking wait inside a tool body. It must be
/// side-effect free and repeat-safe: before the declaring outcome commits,
/// a crash or an attempt fault may run it again.
#[async_trait::async_trait]
pub trait ProcessEnvPolicy: Send + Sync {
    /// # Errors
    ///
    /// [`ProcessEnvPolicyError::Refused`] is the start's tool error, and
    /// [`ProcessEnvPolicyError::Fault`] a retryable attempt fault.
    async fn choose(
        &self,
        cx: &ProcessEnvCx<'_>,
    ) -> Result<ProcessStartChoice, ProcessEnvPolicyError>;
}

/// The definition a start names, read and verified before the policy runs
/// (D8): its engine's authoritative signature, never the model's claim, and
/// the payload the engine runs.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStartTarget {
    pub definition_id: ProcessDefinitionId,
    pub engine_kind: String,
    pub signature: ProcessSignature,
    pub payload: serde_json::Value,
    pub args: serde_json::Map<String, serde_json::Value>,
    /// The effects a run of `payload` performs (P3), as its engine says.
    pub linked: lash_execution_contracts::process::linking::LinkInspection,
}

/// A function a start carries inline, read and checked by its engine before
/// the policy runs (D8, FIG-6382): the definition a start of it runs, and
/// the function itself, which nothing stores until the start is realized.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedInlineStart {
    pub target: ResolvedStartTarget,
    /// The function, as its engine's payload.
    pub request: lash_execution_contracts::process::InlineRequest,
}

/// The process a start is made inside: its recorded environment and
/// settings.
#[derive(Clone, Debug, PartialEq)]
pub struct EnclosingProcess {
    pub process: ProcessId,
    pub env_ref: ProcessEnvRef,
    pub env: ProcessEnv,
    pub execution: lash_core_store::resolutions::ExecutionId,
    pub resolutions: BTreeMap<
        lash_vocabulary::names::OperationUse,
        lash_core_store::resolutions::ResolutionEntry,
    >,
    pub engine_kind: String,
    /// The process's start payload, including its engine's retained Link and
    /// Execution Environment when it uses linked code.
    pub payload: serde_json::Value,
    pub settings: Option<EngineSettings>,
}

/// What a policy decides with.
#[non_exhaustive]
pub struct ProcessEnvCx<'a> {
    pub starter: &'a ExecutionOwner,
    /// D8: authoritative, read and verified before the policy runs.
    pub target: &'a ResolvedStartTarget,
    pub label: Option<&'a str>,
    /// D13: the starting turn's recorded run, never live config. `None`
    /// inside a process.
    pub run: Option<&'a ResolvedRun>,
    /// The recorded surface the declaring attempt was admitted under: what
    /// capture links each performed effect from (R8). `None` inside a
    /// process.
    pub surface: Option<&'a SurfaceTools>,
    pub enclosing: Option<&'a EnclosingProcess>,
    pub execution: Option<&'a lash_core_store::tool_surface::ExecutionEnvironment>,
    pub resolutions: &'a BTreeMap<
        lash_vocabulary::names::OperationUse,
        lash_core_store::resolutions::ResolutionEntry,
    >,
    engines: &'a ProcessEngineRegistry,
}

impl<'a> ProcessEnvCx<'a> {
    pub(crate) fn new(
        starter: &'a ExecutionOwner,
        target: &'a ResolvedStartTarget,
        label: Option<&'a str>,
        run: Option<&'a ResolvedRun>,
        surface: Option<&'a SurfaceTools>,
        enclosing: Option<&'a EnclosingProcess>,
        engines: &'a ProcessEngineRegistry,
    ) -> Self {
        static NO_RESOLUTIONS: BTreeMap<
            lash_vocabulary::names::OperationUse,
            lash_core_store::resolutions::ResolutionEntry,
        > = BTreeMap::new();
        Self {
            starter,
            target,
            label,
            run,
            surface,
            enclosing,
            execution: enclosing.map(|parent| parent.env.execution()),
            resolutions: enclosing.map_or(&NO_RESOLUTIONS, |parent| &parent.resolutions),
            engines,
        }
    }

    pub(crate) fn with_execution(
        mut self,
        environment: Option<&'a lash_core_store::tool_surface::ExecutionEnvironment>,
        resolutions: &'a BTreeMap<
            lash_vocabulary::names::OperationUse,
            lash_core_store::resolutions::ResolutionEntry,
        >,
    ) -> Self {
        if let Some(environment) = environment {
            self.execution = Some(environment);
            self.resolutions = resolutions;
        }
        self
    }

    /// This declaring build's engine defaults for the target, sealed now:
    /// realization never reads them again. An explicit choice.
    ///
    /// # Errors
    ///
    /// The engine is not configured here, or has no defaults for the
    /// payload: a fault, since another build may serve it.
    pub fn engine_defaults(&self) -> Result<Option<EngineSettings>, ProcessEnvPolicyError> {
        self.engines
            .default_settings(&self.target.engine_kind, &self.target.payload)
            .map_err(|error| ProcessEnvPolicyError::Fault(error.to_string()))
    }

    /// Runtime/component facts available to the host's policy implementation.
    pub fn engines(&self) -> &ProcessEngineRegistry {
        self.engines
    }
    pub fn components(&self) -> &crate::plugin::ProcessComponentRegistry {
        self.engines.components()
    }
}

/// A policy's decision: the environment and the engine settings.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessStartChoice {
    pub env: ProcessEnvChoice,
    pub engine_settings: Option<EngineSettings>,
    pub initial_resolutions: Vec<lash_core_store::resolutions::ResolutionSeed>,
}

/// Which environment a start runs under.
#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a choice is made once per start and moved into its launch"
)]
pub enum ProcessEnvChoice {
    /// One already published.
    Published(ProcessEnvRef),
    /// One stated now, admitted and published by the hold.
    New(ProcessEnv),
}

/// Why a policy chose nothing.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ProcessEnvPolicyError {
    #[error(transparent)]
    Package(Box<lash_execution_contracts::process::linking::EnginePortError>),
    /// The start's tool error, model-visible; no intent is declared.
    #[error(transparent)]
    Refused(ProcessEnvRefusal),
    /// A retryable attempt fault; never recorded as a refusal.
    #[error("process start policy fault: {0}")]
    Fault(String),
}
