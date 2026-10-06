//! How a session's scopes end (L6b): its closing state, each step its own
//! fenced, labelled transaction resumed at the step a crash interrupted
//! (ADR 0132 §12), and the turn scopes whose cascade is still marking their
//! `Until` children (§11).
//!
//! The close row is written by [`SessionCloseWrite::Begin`] when a close
//! request is drained, and moves one step at a time. After
//! [`SessionCloseStep::Tombstone`] it is the session's tombstone: the
//! session's other state is gone and its actor is terminal.
//!
//! A turn scope whose first cascade batch did not mark every child is
//! recorded as ending ([`SessionCloseWrite::ScopeEnding`]) in the
//! transaction that ended the turn, and cleared
//! ([`SessionCloseWrite::ScopeEnded`]) in the transaction that marks its last
//! batch, so the session re-drives it on every claim until it is done.

use crate::ids::{CommitLabel, DurableInstant, Epoch};
use lash_sansio::SessionId;

use super::keys::ScopeKey;

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

impl SessionCloseStep {
    /// Every step, in order.
    pub const ALL: [Self; 6] = [
        Self::Cancel,
        Self::Revoke,
        Self::EndScope,
        Self::Triggers,
        Self::Artifacts,
        Self::Tombstone,
    ];

    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Revoke => "revoke",
            Self::EndScope => "end_scope",
            Self::Triggers => "triggers",
            Self::Artifacts => "artifacts",
            Self::Tombstone => "tombstone",
        }
    }

    /// The stored spelling read back; `None` for anything else.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|step| step.as_str() == stored)
    }

    /// The step after this one; `None` after the tombstone.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        Self::ALL
            .into_iter()
            .skip_while(|step| *step != self)
            .nth(1)
    }

    /// The label the step's transaction commits under.
    #[must_use]
    pub const fn label(self) -> CommitLabel {
        match self {
            Self::Cancel => CommitLabel::SESSION_CLOSE_CANCEL,
            Self::Revoke => CommitLabel::SESSION_CLOSE_REVOKE,
            Self::EndScope => CommitLabel::SESSION_CLOSE_END_SCOPE,
            Self::Triggers => CommitLabel::SESSION_CLOSE_TRIGGERS,
            Self::Artifacts => CommitLabel::SESSION_CLOSE_ARTIFACTS,
            Self::Tombstone => CommitLabel::SESSION_CLOSE_TOMBSTONE,
        }
    }
}

/// A closing (or closed) session's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCloseRow {
    /// The session.
    pub session: SessionId,
    /// The last step done; `None` right after the close began.
    pub done: Option<SessionCloseStep>,
    /// When the close began.
    pub begun_at: DurableInstant,
    /// The epoch of the commit that last wrote it.
    pub written_epoch: Epoch,
}

impl SessionCloseRow {
    /// The step to run next; `None` once the tombstone is written.
    #[must_use]
    pub fn next(&self) -> Option<SessionCloseStep> {
        match self.done {
            None => Some(SessionCloseStep::Cancel),
            Some(done) => done.next(),
        }
    }

    /// Whether the close finished: the row is the session's tombstone.
    #[must_use]
    pub fn is_tombstone(&self) -> bool {
        self.done == Some(SessionCloseStep::Tombstone)
    }
}

/// A session-close write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionCloseWrite {
    /// Enter the closing state. A session already closing keeps its row: a
    /// second close request is the same close.
    Begin {
        /// The session.
        session: SessionId,
    },
    /// Record `step` as done. Refused with
    /// [`DomainRefusal::SessionCloseOutOfOrder`](super::DomainRefusal::SessionCloseOutOfOrder)
    /// unless it is the step after the stored one, and with
    /// [`DomainRefusal::SessionNotClosing`](super::DomainRefusal::SessionNotClosing)
    /// when the close never began.
    Step {
        /// The session.
        session: SessionId,
        /// The step done.
        step: SessionCloseStep,
    },
    /// Record that `scope`'s cascade still has children to mark. Recording
    /// a scope already ending changes nothing.
    ScopeEnding {
        /// The session that owns the scope.
        session: SessionId,
        /// The ending scope.
        scope: ScopeKey,
    },
    /// Record that `scope`'s cascade marked its last child.
    ScopeEnded {
        /// The session that owns the scope.
        session: SessionId,
        /// The ended scope.
        scope: ScopeKey,
    },
}
