//! The VM's half of the context (ADR 0132 §8; S7 of I0, FIG-5194). Owned by
//! V0 (FIG-5170), then L7 (FIG-5177).
//!
//! A VM execution (a code cell, or a lashlang process) is filed under its
//! [`ExecKey`]. Its snapshot store, `lash_vm_broker::DurableSnapshotStore`,
//! is built over a context and a key (`DurableSnapshotStore::new(cx, exec)`):
//! the broker sits above this crate, so the store lives there. The snapshot
//! revision, its broker ledger, `admit` + `x_start` of every operation issued
//! since the last snapshot and new waits commit in one
//! `cell.snapshot+admit` transaction; bodies start only after it.

pub use lash_durable::domain::{CellId, ExecKey, SnapshotRev, SnapshotRow};

use super::ActorContext;

/// The VM methods of the context.
impl ActorContext {
    /// The VM effects: `ExecCode` (a cell resumed from its snapshot) and
    /// `LanguageRuntimeValue`. Any other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn vm_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        todo!(
            "L7 (FIG-5177): run a cell from its snapshot, admitting its operations at quiet points"
        )
    }
}
