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
use lash_sansio::{SessionId, TurnId};

/// Where the owners report what hidden replay would make them do.
pub trait DurableProbe: Send + Sync {
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
    fn body_entered(&self, _id: &AdmittedId) {}

    fn vm_program_entered(&self, _exec: &ExecKey) {}

    fn outcome_lookup(&self, _id: &AdmittedId) {}

    fn committed_ordinal_emitted(&self, _owner: &OwnerKey, _run: RunSeq, _ordinal: Ordinal) {}

    fn checkpoint_restored(&self, _session: &SessionId, _run: &TurnId) {}
}
