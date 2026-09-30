//! What the broker asks of the parent: the journaled work behind every
//! admitted operation.
//!
//! The parent's effect bodies stay opaque parent code (ADR 0116): the broker
//! hands an admitted operation, with the identities the parent derived for
//! it, to [`ParentEffects`], which journals and performs it exactly as the
//! parent performs any effect: it dispatches it, or serves the outcome its
//! journal recorded. The broker never runs a tool itself.

use lash_vm_protocol::EffectOutcome;

use crate::authority::RequestFingerprint;
use crate::ledger::AdmittedOperation;

/// What performing an admitted operation answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Performed {
    /// The outcome the worker is answered with.
    pub outcome: EffectOutcome,
    /// A handle the operation's outcome granted (a deferred call), which the
    /// parent scopes to the current frame.
    pub granted: Option<String>,
}

impl Performed {
    pub fn outcome(outcome: EffectOutcome) -> Self {
        Self {
            outcome,
            granted: None,
        }
    }
}

/// A fault of the parent's own journal or store: nothing about the worker or
/// the guest. The run stops and the owning invocation is re-driven.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the parent could not journal the operation: {0}")]
pub struct ParentFault(pub String);

/// The journaled work behind the broker (see the module docs).
#[async_trait::async_trait]
pub trait ParentEffects: Send + Sync {
    /// Retains `operation`'s request under its identity before anything is
    /// dispatched (ADR 0117 §7), and answers the fingerprint the journal
    /// retains there: this one when it is the first, the recorded one on a
    /// replay. The broker refuses the operation when they differ.
    async fn retain(
        &self,
        operation: &AdmittedOperation,
    ) -> Result<RequestFingerprint, ParentFault>;

    /// Performs `operation` through the parent's journal: dispatches it, or
    /// serves the outcome the journal recorded for it.
    async fn perform(&self, operation: &AdmittedOperation) -> Result<Performed, ParentFault>;

    /// The journaled observation of cancellation at instruction checkpoint
    /// `checkpoint` (ADR 0039): recorded once, and served on every replay.
    async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault>;

    /// Whether performing `operation` needs a worker of its own (compiling
    /// model source, running a process body): the run awaiting it then parks
    /// and releases its slot first.
    fn needs_worker(&self, _operation: &AdmittedOperation) -> bool {
        false
    }
}
