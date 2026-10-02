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

/// What performing an operation its run parked on answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParkedPerformed {
    /// The operation ended; the resumed run is answered with its outcome.
    Performed(Performed),
    /// The parent left the operation open for a successor segment: the run
    /// ends [`BrokeredEnd::Suspended`](crate::BrokeredEnd::Suspended) on the
    /// state it parked in.
    HandedOver,
}

/// A fault of the parent's own journal or store: nothing about the worker or
/// the guest. The run stops and the owning invocation is redriven.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the parent could not journal the operation: {0}")]
pub struct ParentFault(pub String);

/// The journaled work behind the broker (see the module docs).
#[async_trait::async_trait]
pub trait ParentEffects: Send + Sync {
    /// Resolve using the parent's admitted routes. Runtime adapters preserve
    /// their existing full authority checks at the effect-body boundary.
    fn resolve(
        &self,
        context: &crate::AdmittedContext,
        grants: &std::collections::BTreeMap<String, crate::HandleGrant>,
        frame: lash_vm_protocol::FrameEpoch,
        request: &lash_vm_protocol::EffectRequest,
    ) -> Result<crate::authority::ResolvedRequest, crate::AuthorityRefusal> {
        crate::authority::resolve(context, grants, frame, request.kind, &request.payload)
    }

    /// The parent's segment decision after a completed effect.
    fn boundary(&self) -> bool {
        false
    }

    fn projection(
        &self,
        _payload: &lash_vm_protocol::EncodedPayload,
    ) -> Result<lash_vm_protocol::EncodedPayload, ParentFault> {
        Err(ParentFault("no projected bindings were admitted".into()))
    }

    fn observe(&self, _payload: &lash_vm_protocol::EncodedPayload) -> Result<(), ParentFault> {
        Ok(())
    }

    fn park_declined(&self, _reason: &str) {}

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

    /// [`perform`](Self::perform) for an operation the run parked on. Its
    /// state is captured, so the parent may leave the operation open for the
    /// segment that resumes the run ([`ParkedPerformed::HandedOver`]). An
    /// operation performed in place never hands over this way: its run holds
    /// no captured state to resume from.
    async fn perform_parked(
        &self,
        operation: &AdmittedOperation,
    ) -> Result<ParkedPerformed, ParentFault> {
        self.perform(operation)
            .await
            .map(ParkedPerformed::Performed)
    }

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
