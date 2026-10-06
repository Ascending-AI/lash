//! The record types an [`ActorContext`](crate::ActorContext) method takes,
//! and the turn-cancel closure binding.
//!
//! The effect-host, controller, scoped-controller and effect-task seams that
//! lived here are gone (ADR 0132 §1; I0, FIG-5194): every effect runs through
//! the concrete `ActorContext`.

pub use lash_core_store::await_event_identity::*;
use std::sync::Arc;

use crate::{ActorContext, RuntimeError, RuntimeErrorCode};

pub use lash_core_effect::CompletionKeyPreparation;
use lash_core_effect::retirement;
pub use retirement::*;

mod journal_guard;
pub use journal_guard::{
    CommandJournalGuard, RecordedKeyFence, RefusedWriteRange, ServedOnlyRange,
};
mod progress;
pub use progress::{BoundaryReason, SegmentProgress};

/// A session catalog's stable registration at the physical promise owner.
///
/// Catalog authorization first registers this participant at the owner and
/// then commits its local pin. Scope retirement reverses that order: it first
/// fences and drains the catalog, then releases the participant.
#[derive(Clone)]
pub struct TurnCancelClosureOwnerBinding {
    participant_id: Arc<str>,
    owner: ActorContext,
}

impl std::fmt::Debug for TurnCancelClosureOwnerBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnCancelClosureOwnerBinding")
            .field("participant_id", &self.participant_id)
            .finish_non_exhaustive()
    }
}

impl TurnCancelClosureOwnerBinding {
    pub fn new(participant_id: impl Into<Arc<str>>, owner: ActorContext) -> Self {
        Self {
            participant_id: participant_id.into(),
            owner,
        }
    }

    pub async fn register(
        &self,
        scope: &ExecutionScope,
        admitted_binding_id: &str,
    ) -> Result<(), RuntimeError> {
        let owner_binding_id =
            super::turn_control_binding_id_for_scope(&self.owner.turn_control_binding_id(), scope)?;
        if owner_binding_id != admitted_binding_id {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidTurnCancelRequest,
                format!(
                    "session catalog cancellation owner `{owner_binding_id}` does not match admitted authority `{admitted_binding_id}`"
                ),
            ));
        }
        self.owner
            .register_turn_cancel_closure_participant(&self.participant_id, scope)
            .await
    }

    pub async fn release(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.owner
            .release_turn_cancel_closure_participant(&self.participant_id, scope)
            .await
    }
}

/// One registry step of a process drive, for
/// [`ActorContext::record_process_drive_step`].
pub type ProcessDriveStep<'step> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), crate::PluginError>> + Send + 'step>,
>;

/// One record of a logical Run, for [`ActorContext::record_run_record`]:
/// the future that produces the record and the canonical material it owns,
/// or a fault that ends the attempt unrecorded.
pub type RunRecordStep<'step> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<lash_core_store::tool_run::RunJournalEntry, String>>
            + Send
            + 'step,
    >,
>;

#[cfg(test)]
#[path = "control/journal_identity_tests.rs"]
mod journal_identity_tests;

/// An artifact-cleanup guard's verdict on one effect journal (ADR 0113 §2.5).
/// No backend journals effects any more (ADR 0132), so a store set answers
/// `Settled`; the verdict goes with journal referrers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalReplay {
    /// The journal may still replay or append.
    MayReplay,
    /// Nothing will replay or append to the journal again.
    Settled,
}
