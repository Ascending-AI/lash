//! [`DurableProbe`]: the replay tripwire's seam (ADR 0132 §2, laws NR-1 to
//! NR-4).
//!
//! Owners call it where hidden replay would show: a body entered, a VM
//! program started from instruction 0, an outcome looked up on behalf of
//! re-running code, a committed `RunLedger` ordinal emitted again, and a turn
//! restored from its checkpoint. Production passes [`NoProbe`]; the harness
//! passes `lash_durable_test::Tripwire`, which counts each per identity.
//!
//! No method has a default body: a probe states what it does with each.

use crate::domain::{AdmittedId, ExecKey, Ordinal, OwnerKey, RunSeq};
use crate::{ActorKey, CommitLabel, DurableError, Epoch};
use lash_vocabulary::identity::{SessionId, TurnId};

/// Owner reads attributed to one actor epoch. Hydration and requested
/// context are counted separately from repeated reads of acknowledged state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OwnerRead {
    OwnHeadReload,
    OwnCommittedPhaseReload,
    ExternalFrontierReads,
    RecoveryLoads,
    HistoricalPinReads,
    OwnProcessRecordReload,
    OwnProcessDriverReload,
    OwnProcessRootReload,
    CompletedRunRowsReplayed,
    ContextBytes,
    ActiveRunRows,
    VmPages,
}

/// Memory-only cuts after durable success. A failed hook invalidates the
/// entire owner; it cannot turn a committed transition into a rollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OwnerCut {
    /// Receipt received, before any candidate is adopted.
    Acknowledged,
    /// Adoption complete, before publication or subsequent effects.
    Adopted,
}

/// Where the owners report what hidden replay would make them do.
pub trait DurableProbe: Send + Sync {
    /// Account a read at its source, after the claim's initial hydration.
    fn owner_read(&self, actor: &ActorKey, epoch: Epoch, event: OwnerRead, amount: u64);

    /// Observe or interrupt a post-commit memory transition. Production is
    /// a no-op; harness failures must still poison the owner.
    fn owner_cut(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
        label: CommitLabel,
        cut: OwnerCut,
    ) -> Result<(), DurableError>;

    /// An admitted execution's body was entered.
    fn body_entered(&self, id: &AdmittedId);

    /// A VM program was entered fresh, from instruction 0, rather than
    /// resumed from a snapshot.
    fn vm_program_entered(&self, exec: &ExecKey);

    /// An outcome was looked up on behalf of code that is running again.
    fn outcome_lookup(&self, id: &AdmittedId);

    /// A producer emitted a `RunLedger` ordinal that is already committed.
    fn committed_ordinal_emitted(&self, owner: &OwnerKey, run: RunSeq, ordinal: Ordinal);

    /// A turn was restored from its checkpoint.
    fn checkpoint_restored(&self, session: &SessionId, run: &TurnId);
}

/// The production probe: it notes nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProbe;

impl DurableProbe for NoProbe {
    fn owner_read(&self, _actor: &ActorKey, _epoch: Epoch, _event: OwnerRead, _amount: u64) {}

    fn owner_cut(
        &self,
        _actor: &ActorKey,
        _epoch: Epoch,
        _label: CommitLabel,
        _cut: OwnerCut,
    ) -> Result<(), DurableError> {
        Ok(())
    }

    fn body_entered(&self, _id: &AdmittedId) {}

    fn vm_program_entered(&self, _exec: &ExecKey) {}

    fn outcome_lookup(&self, _id: &AdmittedId) {}

    fn committed_ordinal_emitted(&self, _owner: &OwnerKey, _run: RunSeq, _ordinal: Ordinal) {}

    fn checkpoint_restored(&self, _session: &SessionId, _run: &TurnId) {}
}
