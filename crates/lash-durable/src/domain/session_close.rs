//! The session's closing state (L6b): each step its own fenced, labelled
//! transaction, resumed at the step a crash interrupted (ADR 0132 §12).

use lash_sansio::SessionId;

/// One step of a session close, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SessionCloseStep {
    /// Cancel the open turn.
    Cancel,
    /// Revoke the session's waits.
    Revoke,
    /// End the session's `Until` processes, batched.
    EndScope,
    /// Delete the session's triggers.
    Triggers,
    /// Arm the session's artifact cleanup.
    Artifacts,
    /// Write the tombstone and delete the session's state.
    Tombstone,
}

/// A session-close write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionCloseWrite {
    /// Enter the closing state.
    Begin {
        /// The session.
        session: SessionId,
    },
    /// Record `step` as done.
    Step {
        /// The session.
        session: SessionId,
        /// The step done.
        step: SessionCloseStep,
    },
}
