use super::*;
use lash_kernel_doc::EffectName;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// The mail kind that carries a [`ProcessSuccessorRequest`] to the process's
/// actor.
pub const SUCCESSOR_MAIL: &str = "process.successor";

/// The pair a host names for a process to go on as: the engine start
/// payload and the published env it runs under, with the fit each effect's
/// link is checked at (the default is [`FitPolicy::Compatible`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSuccessor {
    pub payload: serde_json::Value,
    pub env: ProcessEnvRef,
}

impl ProcessSuccessor {
    #[must_use]
    pub fn new(payload: serde_json::Value, env: ProcessEnvRef) -> Self {
        Self { payload, env }
    }
}

/// A successor request as it is recorded: the mail's body, and the
/// driver's pending request until its owner applies it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSuccessorRequest {
    pub payload: serde_json::Value,
    pub env: ProcessEnvRef,
    /// The definition the engine's admission names the new payload by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_id: Option<ProcessDefinitionId>,
}

impl ProcessSuccessorRequest {
    /// The replay key of the event `prefix` records this request as.
    #[must_use]
    pub(crate) fn replay_key(&self, process: &ProcessId, prefix: &str) -> String {
        let digest = crate::stable_hash::sha256_hex(&crate::identity_json::payload_leaf(
            &serde_json::to_value(self).unwrap_or_default(),
        ));
        format!("{prefix}:{process}:{digest}")
    }
}

/// What a successor request answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessSuccession {
    /// It was recorded: the process's owner applies it once the run is at
    /// rest, or records why it refused.
    Requested,
    /// The process already runs the named pair; nothing was recorded.
    AlreadyCurrent,
}

/// Why a successor request is refused. A refusal records nothing, and the
/// process goes on as written.
#[derive(
    Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
#[serde(tag = "refused", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ProcessSuccessorRefusal {
    #[error("no such process")]
    UnknownProcess,
    #[error("the process has ended")]
    Ended,
    #[error("the process runs no engine")]
    NotEngine,
    #[error("process engine `{kind}` carries no process onto a successor")]
    Unsupported { kind: String },
    /// The new pair does not link (P1): items 17 and 21.
    #[error("the successor pair does not link: {refusal}")]
    Link { refusal: ProcessEnvRefusal },
    /// The new env links `effect`, which an admitted step of the process
    /// performs, otherwise than the step's admission matched it.
    #[error("an admitted step performs `{effect}`, which the successor env links otherwise")]
    AdmittedStepRelinked { effect: String },
    #[error("the process is being cancelled")]
    Cancelling,
    /// The engine's own reason, as its data: for a kernel process, why no
    /// correspondence carries its run onto the new document.
    #[error("{message}")]
    Engine {
        refusal: serde_json::Value,
        message: String,
    },
}

/// What an engine's [`EngineSuccession::carry`] answers.
#[derive(Clone, Debug, PartialEq)]
pub enum SuccessorCarry {
    /// The state carried onto the successor payload; `None` for a process
    /// whose engine has not started.
    Carried {
        state: Option<Box<EngineState>>,
        /// The carried state pins what this build's engine writes, such as
        /// its own helper release: the process's actor is stamped with the
        /// engine's own format set, so only a node that holds them claims
        /// it from now on.
        own_formats: bool,
    },
    /// The run is in flight: it is carried at its next park.
    InFlight,
}

/// Why an engine did not carry a state onto a successor.
#[derive(Clone, Debug, PartialEq)]
pub enum SuccessionError {
    /// The engine refuses, typed: no correspondence carries the run.
    Refused {
        refusal: serde_json::Value,
        message: String,
    },
    /// A store did not answer; asked again later.
    Unavailable(String),
}

/// How an engine carries a process onto a successor payload its host
/// names (D2): the engine's own correspondence carry. Pure over what it
/// reads; it writes nothing, and the process's owner commits its answer.
#[async_trait::async_trait]
pub trait EngineSuccession: Send + Sync {
    /// `state`, a run of `from` (`None` before the engine's first
    /// transition), carried onto `to`.
    async fn carry(
        &self,
        process: &ProcessId,
        from: &serde_json::Value,
        to: &serde_json::Value,
        state: Option<&EngineState>,
        environment: &ProcessEnv,
    ) -> Result<SuccessorCarry, SuccessionError>;
}

/// Whether `record` already runs this pair.
#[must_use]
pub fn successor_is_current(request: &ProcessSuccessorRequest, record: &ProcessRecord) -> bool {
    match record.input.as_ref() {
        ProcessInput::Engine {
            payload, launch, ..
        } => *payload == request.payload && launch.env == request.env,
        ProcessInput::SessionTurn { .. } => false,
    }
}

/// Linked-use succession; implementation belongs to pkg-doc and pkg-exec.
pub mod linked {
    use super::SuccessionError;
    use crate::process::engine_state::linked::EngineState;
    use lash_core_store::artifact_referrer::ArtifactName;
    use lash_core_store::{resolutions::*, tool_surface::ExecutionEnvironment};
    use lash_vocabulary::{claims::*, identity::ProcessId, names::*};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    /// Total mapping of live/held uses. Many-to-one requires equal requirements and pinned outcomes.
    pub struct UseCorrespondence {
        #[serde(with = "lash_vocabulary::ordered_pairs")]
        pub uses: BTreeMap<OperationUse, OperationUse>,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    /// Stable execution, predecessor/successor starts, accepted environment and retained outcomes; never re-resolved.
    pub struct SuccessionContext {
        pub execution: ExecutionId,
        pub from: LinkedStart,
        pub to: LinkedStart,
        pub environment: ExecutionEnvironment,
        #[serde(with = "lash_vocabulary::ordered_pairs")]
        pub resolutions: BTreeMap<OperationUse, ResolutionEntry>,
    }
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(
        tag = "kind",
        content = "value",
        rename_all = "snake_case",
        deny_unknown_fields
    )]
    pub enum SuccessorCarry {
        Carried {
            state: Option<Box<EngineState>>,
            own_formats: bool,
            correspondence: UseCorrespondence,
            resolutions: Vec<ResolutionRecord>,
            requirements: Box<ClaimRequirements>,
            artifacts: Vec<ArtifactName>,
        },
        InFlight,
    }
    #[async_trait::async_trait]
    /// Runtime computes correspondence; Core atomically validates/holds/copies state, environment, outcomes and debt.
    pub trait EngineSuccession: Send + Sync {
        async fn carry(
            &self,
            process: &ProcessId,
            context: &SuccessionContext,
            state: Option<&EngineState>,
        ) -> Result<SuccessorCarry, SuccessionError>;
    }
}
