//! How a host process engine's steps run (ADR 0132 §10; L6, FIG-5175).
//!
//! A [`StepRequest`] names a catalog tool, or one of the engine's own bodies
//! that its registration declares ([`EngineSteps`](super::EngineSteps)). The
//! process actor admits it as an admitted execution (S4) under the tool's
//! declared [`ExecutionPolicy`] (an engine body's is a pinned `Repeatable`)
//! and an [`ExecutionLimit`] within the tool ceiling, commits that admission
//! with the state that asked for it, and only then runs the body. A step
//! runs the admitted-execution lifecycle a round member runs
//! ([`lifecycle`](crate::runtime::actor::round::lifecycle)): a `Repeatable`
//! failure its contract repeats is retried at the run's next ordinal, and a
//! step that may park has its completion wait pinned with its admission
//! and settles from that wait's resolution. The host supplies every half
//! through [`ProcessSteps`]; there is no default.

use lash_core_store::tool_run::CompletionSource;
use lash_sansio::{ExecutionLimit, ExecutionPolicy};

use super::engine_state::StepRequest;
use crate::ProcessRecord;
use crate::runtime::actor::round::{AdmittedExecution, Material, SettledOutput, ToolBody};
use crate::runtime::actor::waits::{Resolution, WaitDeadline};

/// A step's admission: what its tool's declaration pins before it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepAdmission {
    /// The tool's execution policy.
    pub policy: ExecutionPolicy,
    /// The step's limit, within the tool ceiling.
    pub limit: ExecutionLimit,
    /// For a step that may park, the deadline of the completion wait its
    /// admission pins: its park never outlives it.
    pub wait: Option<WaitDeadline>,
}

/// A step refused before its admission; nothing was recorded. The process
/// ends `Failed` with it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StepRefusal {
    /// The step names a tool the process's catalog does not hold.
    #[error("process step `{step}` names `{tool}`, which the process's catalog does not hold")]
    UnknownTool {
        /// The step.
        step: String,
        /// The tool it names.
        tool: String,
    },
    /// The tool's declaration refuses the step: its input does not match,
    /// or its declared execution is above the ceiling.
    #[error("process step `{step}` is refused by its tool: {reason}")]
    Refused {
        /// The step.
        step: String,
        /// Why.
        reason: String,
    },
    /// The step names an engine body the process's engine does not declare.
    #[error(transparent)]
    Engine(#[from] super::engine_state::EngineStepRefusal),
}

/// The host's half of a process step: the catalog tool's declaration and
/// body, or the engine body its registration declares
/// ([`ProcessEngineRegistry::engine_steps`](super::ProcessEngineRegistry::engine_steps)).
/// A durable backend serving processes is given exactly one.
pub trait ProcessSteps: Send + Sync {
    /// Admit `step` of `process` at `now_ms`: the policy its tool declares
    /// and the limit it runs under.
    ///
    /// # Errors
    ///
    /// [`StepRefusal`]; nothing is recorded.
    fn admit(
        &self,
        process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal>;

    /// The body of `execution`, an attempt of the admitted `step` of
    /// `process`. It runs only after its admission (or its retry's start)
    /// committed, under its limit and the cancel token it is handed. A step
    /// admitted with a wait parks by answering `Waiting` on the wait its
    /// execution pinned ([`ExecutionDraft::pinned_wait`]).
    ///
    /// [`ExecutionDraft::pinned_wait`]: crate::runtime::actor::round::ExecutionDraft::pinned_wait
    fn body(
        &self,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &AdmittedExecution,
    ) -> ToolBody;

    /// The final answer of `execution`, an attempt of `step` of `process`
    /// that parked as `parked`, once one of its waits ended with
    /// `resolution`: a pure function of the resolution and the payload of
    /// the parked outcome's material. Runs no body.
    fn resolved(
        &self,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput;
}
