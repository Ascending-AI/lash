//! Turn rows (V0, then L3) and turn cancel requests (L3).
//!
//! A turn is a sequence of committed phases of the sans-io `TurnMachine`
//! (ADR 0132 §4). Its row names the phase, the encoded checkpoint, the
//! pinned model request and the terminal. At most one turn per session is
//! unfinished.

use crate::ids::{DurableInstant, Epoch};
use lash_sansio::{SessionId, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId};

use super::keys::RunSeq;

/// Where a turn is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnPhase {
    /// Admitted: inputs bound, deadline recorded.
    Admitted,
    /// Prepared: the prepared context and checkpoint committed.
    Prepared,
    /// A model call is in flight under `attempt`, its request pinned.
    Model {
        /// The attempt, from 1.
        attempt: u32,
    },
    /// A tool round is admitted and its bodies may run.
    Tools {
        /// The round's run.
        run: RunSeq,
    },
    /// Blocked on waits or timers.
    Waiting,
    /// Committing the turn.
    Committing,
    /// Ended.
    Terminal(TurnTerminal),
}

impl TurnPhase {
    /// The stored spelling and its argument: a tool round's run or a model
    /// call's attempt. A terminal phase is stored on the turn's run row,
    /// never as a phase.
    #[must_use]
    pub fn stored(&self) -> (&'static str, Option<u64>) {
        match self {
            Self::Admitted => ("admitted", None),
            Self::Prepared => ("prepared", None),
            Self::Model { attempt } => ("model", Some(u64::from(*attempt))),
            Self::Tools { run } => ("tools", Some(run.0)),
            Self::Waiting => ("waiting", None),
            Self::Committing => ("committing", None),
            Self::Terminal(_) => ("terminal", None),
        }
    }

    /// A stored phase read back; `None` for anything [`Self::stored`] does
    /// not write.
    #[must_use]
    pub fn parse(stored: &str, argument: Option<u64>) -> Option<Self> {
        Some(match (stored, argument) {
            ("admitted", None) => Self::Admitted,
            ("prepared", None) => Self::Prepared,
            ("model", Some(attempt)) => Self::Model {
                attempt: u32::try_from(attempt).ok()?,
            },
            ("tools", Some(run)) => Self::Tools { run: RunSeq(run) },
            ("waiting", None) => Self::Waiting,
            ("committing", None) => Self::Committing,
            _ => return None,
        })
    }
}

impl TurnTerminal {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The terminal its stored spelling names.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        [Self::Answered, Self::Failed, Self::Cancelled]
            .into_iter()
            .find(|terminal| terminal.as_str() == stored)
    }
}

/// How a turn ended (L3): its terminal, its typed cause, and the head
/// revision its commit published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnEnd {
    /// The terminal.
    pub terminal: TurnTerminal,
    /// Its typed cause, encoded by its owner.
    pub cause_json: Option<String>,
    /// The head revision its commit published; `None` when it published
    /// none (a cancelled turn).
    pub head_revision: Option<u64>,
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnTerminal {
    /// It answered.
    Answered,
    /// It failed.
    Failed,
    /// It was cancelled.
    Cancelled,
}

/// A pinned model request: re-sent byte-identical as the next attempt while
/// its deadline allows. The deadline is written before the first byte is
/// sent and never refreshed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPin {
    /// The attempt, from 1.
    pub attempt: u32,
    /// The pinned request, encoded by its owner.
    pub request_ref: String,
    /// The `model_total` deadline.
    pub deadline: DurableInstant,
}

/// One turn's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnRow {
    /// The session.
    pub session: SessionId,
    /// The run (the turn's identity).
    pub run: TurnId,
    /// The admission: bound inputs, base head revision, run spec, plugin
    /// revision, encoded by its owner.
    pub admission_json: String,
    /// The phase.
    pub phase: TurnPhase,
    /// The protocol iteration: the model-call ordinal.
    pub iteration: u32,
    /// The bounded `TurnCheckpoint` (L3a's `SavedTurn`), encoded inline by
    /// its owner.
    pub checkpoint_ref: Option<String>,
    /// The in-flight model call.
    pub model: Option<ModelPin>,
    /// The host's turn deadline, recorded at admission.
    pub turn_deadline: Option<DurableInstant>,
    /// The epoch of the commit that last wrote the row.
    pub written_epoch: Epoch,
    /// The cancel request the turn accepted, if any (L3).
    pub cancel: Option<TurnCancelRequest>,
}

/// A turn-row write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnWrite {
    /// Insert an admitted turn. Refused with
    /// [`DomainRefusal::OpenTurnExists`](super::DomainRefusal::OpenTurnExists)
    /// when the session has an unfinished one.
    Admit {
        /// The session.
        session: SessionId,
        /// The run.
        run: TurnId,
        /// The admission.
        admission_json: String,
        /// The host's turn deadline.
        turn_deadline: Option<DurableInstant>,
    },
    /// Move an unfinished turn to `phase` with its checkpoint and model pin.
    /// Refused with
    /// [`DomainRefusal::TurnNotOpen`](super::DomainRefusal::TurnNotOpen) when
    /// it is not the session's unfinished turn.
    Advance {
        /// The session.
        session: SessionId,
        /// The run.
        run: TurnId,
        /// The new phase; never [`TurnPhase::Terminal`].
        phase: TurnPhase,
        /// The protocol iteration.
        iteration: u32,
        /// The checkpoint, encoded inline by its owner.
        checkpoint_ref: Option<String>,
        /// The in-flight model call, or `None` once it is done.
        model: Option<ModelPin>,
    },
    /// End the turn and drop its phase row. Refused with
    /// [`DomainRefusal::TurnNotOpen`](super::DomainRefusal::TurnNotOpen) when
    /// it is not the session's unfinished turn.
    Terminal {
        /// The session.
        session: SessionId,
        /// The run.
        run: TurnId,
        /// How it ended.
        terminal: TurnTerminal,
        /// Its typed cause, encoded by its owner.
        cause_json: Option<String>,
        /// The head revision its commit published, if it published one.
        head_revision: Option<u64>,
    },
}

/// The turn's commit to its session (V0, then L3): the session store's own
/// head commit, applied inside the `turn.commit` transaction after the
/// fence. It publishes revision `expected_head + 1` and moves the session
/// head to it, with everything the session store writes beside a head (the
/// turn's history nodes, checkpoint, receipt); a session with no head is at
/// revision 0. The head compare-and-set makes a repeated commit refuse
/// rather than publish twice, and the turn's phase row is dropped by its
/// [`TurnWrite::Terminal`] in the same transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCommitWrite {
    /// The session.
    pub session: SessionId,
    /// The run.
    pub run: TurnId,
    /// The head revision the commit replaces. Refused with
    /// [`DomainRefusal::HeadMoved`](super::DomainRefusal::HeadMoved) when
    /// the head is elsewhere.
    pub expected_head: u64,
    /// The session store's commit, encoded by its owner (lash-core-store's
    /// `encode_session_commit`). Refused with
    /// [`DomainRefusal::SessionCommitRefused`](super::DomainRefusal::SessionCommitRefused)
    /// when the session store refuses it.
    pub commit_json: String,
}

/// A request to cancel one of a session's turns (L3).
///
/// It is not a mail row: a [`MailDomainWrite::RequestTurnCancel`](super::MailDomainWrite::RequestTurnCancel)
/// records it on the turn's cancel-request row and control-wakes the session
/// in the producer's mailbox transaction. The session's owner reads it back
/// on the turn's row ([`TurnRow::cancel`]) at its next fenced read, so a
/// request survives the owner that saw it: the next owner finalizes the turn.
///
/// The first request a turn accepts holds the undelivered-input policy; a
/// later one with the same policy and a stronger mode escalates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCancelRequest {
    /// The session.
    pub session: SessionId,
    /// The turn to cancel.
    pub run: TurnId,
    /// The host's request id.
    pub request_id: String,
    /// Opaque host-domain data, recorded and returned unchanged.
    pub origin: Option<String>,
    /// The host's reason.
    pub reason: Option<String>,
    /// What becomes of input the turn did not deliver.
    pub undelivered: TurnCancelUndeliveredInputPolicy,
    /// When the owner honours it.
    pub mode: TurnCancelMode,
}

impl TurnCancelRequest {
    /// Whether this request escalates `accepted`: the same policy with a
    /// stronger mode. A request that disagrees about the policy never does.
    #[must_use]
    pub fn escalates(&self, accepted: &Self) -> bool {
        self.undelivered == accepted.undelivered && self.mode.is_stronger_than(accepted.mode)
    }
}

/// The answer to a [`TurnCancelRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnCancelAnswer {
    /// The turn's first request: recorded, and the session woken.
    Requested,
    /// The accepted request took this request's stronger mode, and the
    /// session was woken. `accepted` is the request as it now stands.
    Escalated {
        /// The accepted request, escalated.
        accepted: TurnCancelRequest,
    },
    /// The turn already holds a request with the same policy and a mode at
    /// least as strong; nothing was written.
    AlreadyRequested {
        /// The accepted request.
        accepted: TurnCancelRequest,
    },
    /// The turn already accepted another undelivered-input policy; nothing
    /// was written.
    PolicyConflict {
        /// The accepted request.
        accepted: TurnCancelRequest,
    },
    /// The turn is not the session's unfinished turn; nothing was written.
    AlreadyEnded,
}
