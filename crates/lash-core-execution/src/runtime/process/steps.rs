//! How a host process engine's steps run (ADR 0132 §10; L6, FIG-5175).
//!
//! A [`StepRequest`] names a catalog tool. The process actor admits it as an
//! admitted execution (S4) under the tool's declared [`ExecutionPolicy`] and
//! an [`ExecutionLimit`] within the tool ceiling, commits that admission with
//! the state that asked for it, and only then runs the tool's body. The host
//! supplies both halves through [`ProcessSteps`]; there is no default.

use lash_sansio::{ExecutionLimit, ExecutionPolicy};

use super::engine_state::StepRequest;
use crate::runtime::actor::round::ToolBody;
use crate::{ProcessRecord, ToolCallId};

/// A step's admission: what its tool's declaration pins before it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepAdmission {
    /// The tool's execution policy.
    pub policy: ExecutionPolicy,
    /// The step's limit, within the tool ceiling.
    pub limit: ExecutionLimit,
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
}

/// The host's half of a process step: the catalog tool's declaration and
/// body. A durable backend serving processes is given exactly one.
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

    /// The body of an admitted `step` of `process`, named `call`. It runs
    /// only after its admission committed, under its limit and the cancel
    /// token it is handed.
    fn body(&self, process: &ProcessRecord, step: &StepRequest, call: &ToolCallId) -> ToolBody;
}
