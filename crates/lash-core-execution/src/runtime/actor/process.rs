//! The process's half of the context (ADR 0132 §10, §11). Owned by L6
//! (FIG-5175).
//!
//! The process activation, `advance` driving and the terminal transaction
//! live here; the engine trait is `runtime::process::engine::ProcessEngine`.

use lash_durable::ActorTx;
use lash_durable::domain::ScopeKey;

pub use lash_durable::domain::{CancelAnswer, CancelRequest, ProcessActorRow};

/// How far one [`end_scope`] batch got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CascadeProgress {
    /// Every `Until` child of the scope is marked for cancel.
    Done,
    /// A batch was marked; the rest continue from the durable cursor.
    More {
        /// How many children this batch marked.
        marked: usize,
    },
}

/// Mark the next `batch` of `scope`'s `Until` children for cancel on `tx`,
/// keeping a durable cursor for the rest. A turn's commit, a session close
/// and a process terminal call it.
pub fn end_scope(_tx: &mut ActorTx, _scope: ScopeKey, _batch: usize) -> CascadeProgress {
    todo!("L6 (FIG-5175): mark a batch of a scope's Until children for cancel")
}

use super::ActorContext;

/// The process methods of the context.
impl ActorContext {
    /// The process effects: `Process` (start, list, transfer, await,
    /// attach, cancel, signal, emit) and `LoadExecutionEnv`. A start is a
    /// store-local effect of its call's outcome; an await is a
    /// `process_terminal` wait; a cancel is mail. Any other command is
    /// refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn process_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        todo!("L6 (FIG-5175): run a process effect as a store-local effect, wait or mail")
    }

    /// Whether the process this context runs has a committed cancellation.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn observe_process_cancel(
        &self,
        _lent_stop: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::RuntimeEffectControllerError> {
        todo!(
            "L6 (FIG-5175): read the process's cancel_requested_at, or delete where advance replaces it"
        )
    }

    /// Run one registry step of a process drive.
    ///
    /// # Errors
    ///
    /// The step's refusal.
    pub async fn record_process_drive_step(
        &self,
        _name: String,
        _step: crate::ProcessDriveStep<'_>,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        todo!(
            "L6 (FIG-5175): delete where advance replaces it, else a write under the process epoch"
        )
    }
}
