//! The VM's half of the context (ADR 0132 §8; S7 of I0, FIG-5194). Owned by
//! V0 (FIG-5170), then L7 (FIG-5177).
//!
//! A VM execution (a code cell, or a lash_vm process) is filed under its
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
    /// The VM effects, run in place and recorded nowhere. A code cell
    /// (`ExecCode`) is durable through its own snapshot: its executor resumes
    /// it from `DurableSnapshotStore::latest` and admits each operation it
    /// issues at a quiet point. A `LanguageRuntimeValue` is computed where it
    /// is asked: a cell resumed from its snapshot never asks again for a value
    /// its heap already holds. Any other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal, or the body's.
    pub async fn vm_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        match &envelope.command {
            crate::RuntimeEffectCommand::ExecCode { .. }
            | crate::RuntimeEffectCommand::LanguageRuntimeValue { .. } => {
                local.run_in_place(envelope).await
            }
            other => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!("{:?} is not a VM effect", other.kind()),
            )),
        }
    }
}
