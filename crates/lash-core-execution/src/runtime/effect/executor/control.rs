//! The record types an [`ActorContext`](crate::ActorContext) method takes.
//!
//! The effect-host, controller, scoped-controller and effect-task seams that
//! lived here are gone (ADR 0132 §1; I0, FIG-5194): every effect runs through
//! the concrete `ActorContext`.

pub use lash_core_store::await_event_identity::*;

pub use lash_core_effect::CompletionKeyPreparation;
use lash_core_effect::retirement;
pub use retirement::*;

mod journal_guard;
pub use journal_guard::{
    CommandJournalGuard, RecordedKeyFence, RefusedWriteRange, ServedOnlyRange,
};
mod progress;
pub use progress::{BoundaryReason, SegmentProgress};

/// One registry step of a process drive, for
/// [`ActorContext::record_process_drive_step`].
pub type ProcessDriveStep<'step> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), crate::PluginError>> + Send + 'step>,
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
