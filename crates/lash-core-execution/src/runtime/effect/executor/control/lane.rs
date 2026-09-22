//! The durable session-execution lane a queued drain must own, and the
//! tool-intent submission gate that rides beside it.
//!
//! Split out of `control.rs` verbatim to keep every file in this module under
//! the production file-size budget; no item, signature or path changed.

use super::*;

pub use lash_core_effect::queued_lane::{
    CompletionKeyPreparation, QueuedLaneAcquisition, QueuedLaneAttempt, QueuedLaneGuard,
    QueuedLaneHolder, QueuedLaneProbe,
};

/// Opaque guard holding one facade tool-intent submission gate.
#[allow(dead_code)]
pub struct ToolIntentSubmissionGuard(tokio::sync::OwnedMutexGuard<()>);

impl ToolIntentSubmissionGuard {
    /// Wrap the owned mutex guard supplied by the facade's submission-gate
    /// collaborator.
    pub fn from_owned_mutex_guard(guard: tokio::sync::OwnedMutexGuard<()>) -> Self {
        Self(guard)
    }
}

/// Core-provided collaborator for durable tool-intent admission and outcome
/// recording. The effect-host seam never learns which registry or lock table
/// backs these operations.
#[async_trait::async_trait]
pub trait ToolIntentOutcomeSink: Send + Sync {
    async fn lock_submission_gate(&self, replay_key: &str) -> ToolIntentSubmissionGuard;

    async fn admit(
        &self,
        record: crate::ToolIntentSubmissionRecord,
    ) -> Result<crate::ToolIntentSubmissionAdmission, RuntimeError>;

    async fn complete_submission(
        &self,
        identity: &crate::ToolIntentIdentity,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError>;

    async fn retain_in_journal(
        &self,
        identity: &crate::ToolIntentIdentity,
        submitted: crate::ToolIntent,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError>;
}

/// Preparation chosen by the effect host before one tool intent is realized.
pub enum ToolIntentPreparation {
    /// The controller journal owns the submission.
    ControllerOwned,
    /// The runtime registry owns the submission row and the gate remains held
    /// until realization and outcome recording finish.
    RuntimeOwned {
        admission: crate::ToolIntentSubmissionAdmission,
        _guard: ToolIntentSubmissionGuard,
    },
}
