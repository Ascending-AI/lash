//! Turn rows (V0, then L3) and turn cancel requests (L3).
//!
//! A turn is a sequence of committed phases of the sans-io `TurnMachine`
//! (ADR 0132 §4). Its row names the phase, the encoded checkpoint, the
//! pinned model request and the terminal. At most one turn per session is
//! unfinished.

use crate::ids::{DurableInstant, Epoch};
use lash_core_store::store::{RunAdmissionRecord, RunTerminalCause, RunTerminalKind};
use lash_sansio::{InputId, SessionId, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId};

use super::keys::RunSeq;

/// Where an unfinished turn is: each phase carries exactly what a restore
/// resumes it from. A turn's end is its run's terminal, never a phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnfinishedPhase {
    /// Admitted: inputs bound, deadline recorded, nothing run yet. A
    /// restore starts the turn from its admission.
    Admitted,
    /// A model call is in flight, its request pinned before its first byte.
    Model {
        /// The pinned call.
        pin: ModelPin,
        /// The checkpoint that re-delivers the call, encoded inline by its
        /// owner.
        checkpoint: String,
    },
    /// A tool round or a code cell is admitted and its bodies may run.
    Tools {
        /// The round's run.
        run: RunSeq,
        /// The checkpoint that re-delivers the round, encoded inline by its
        /// owner.
        checkpoint: String,
    },
}

impl UnfinishedPhase {
    /// The stored spelling and its argument: a model call's attempt or a
    /// tool round's run.
    #[must_use]
    pub fn stored(&self) -> (&'static str, Option<u64>) {
        match self {
            Self::Admitted => ("admitted", None),
            Self::Model { pin, .. } => ("model", Some(u64::from(pin.attempt))),
            Self::Tools { run, .. } => ("tools", Some(run.0)),
        }
    }

    /// The checkpoint a restore resumes from; `None` while admitted.
    #[must_use]
    pub fn checkpoint(&self) -> Option<&str> {
        match self {
            Self::Admitted => None,
            Self::Model { checkpoint, .. } | Self::Tools { checkpoint, .. } => Some(checkpoint),
        }
    }

    /// The pinned model call; `Some` exactly in the model phase.
    #[must_use]
    pub fn model(&self) -> Option<&ModelPin> {
        match self {
            Self::Model { pin, .. } => Some(pin),
            Self::Admitted | Self::Tools { .. } => None,
        }
    }

    /// The phase its stored columns name; `None` for anything
    /// [`Self::stored`] does not write. The DDL refuses those rows too.
    #[must_use]
    pub fn parse(
        stored: &str,
        argument: Option<u64>,
        checkpoint: Option<String>,
        pin: Option<(String, DurableInstant)>,
    ) -> Option<Self> {
        Some(match (stored, argument, checkpoint, pin) {
            ("admitted", None, None, None) => Self::Admitted,
            ("model", Some(attempt), Some(checkpoint), Some((request_ref, deadline))) => {
                Self::Model {
                    pin: ModelPin {
                        attempt: u32::try_from(attempt).ok()?,
                        request_ref,
                        deadline,
                    },
                    checkpoint,
                }
            }
            ("tools", Some(run), Some(checkpoint), None) => Self::Tools {
                run: RunSeq(run),
                checkpoint,
            },
            _ => return None,
        })
    }
}

/// How a turn ended (L3): its run's typed terminal cause and the head
/// revision its commit published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnEnd {
    /// Why the run ended.
    pub cause: RunTerminalCause,
    /// The head revision its commit published; `None` when it published
    /// none (a cancelled turn).
    pub head_revision: Option<u64>,
}

impl TurnEnd {
    /// How it ended: its cause's kind, never a second fact.
    #[must_use]
    pub fn kind(&self) -> RunTerminalKind {
        self.cause.kind()
    }
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
    /// What the run's admission took.
    pub admission: RunAdmissionRecord,
    /// The phase, with what a restore resumes it from.
    pub phase: UnfinishedPhase,
    /// The protocol iteration: the model-call ordinal.
    pub iteration: u32,
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
        /// What the admission took.
        admission: RunAdmissionRecord,
        /// The host's turn deadline.
        turn_deadline: Option<DurableInstant>,
    },
    /// Move an unfinished turn to `phase`. Refused with
    /// [`DomainRefusal::TurnNotOpen`](super::DomainRefusal::TurnNotOpen) when
    /// it is not the session's unfinished turn.
    Advance {
        /// The session.
        session: SessionId,
        /// The run.
        run: TurnId,
        /// The new phase.
        phase: UnfinishedPhase,
        /// The protocol iteration.
        iteration: u32,
    },
    /// End the turn and drop its phase row. Refused with
    /// [`DomainRefusal::TurnNotOpen`](super::DomainRefusal::TurnNotOpen) when
    /// it is not the session's unfinished turn.
    Terminal {
        /// The session.
        session: SessionId,
        /// The run.
        run: TurnId,
        /// Why it ended; its kind is the stored terminal.
        cause: Box<RunTerminalCause>,
        /// The head revision its commit published, if it published one.
        head_revision: Option<u64>,
    },
}

/// A session's head commit from its own actor (V0, then L3; FIG-5230): the
/// session store's own head commit, applied inside the owner's fenced
/// transaction after the fence. A turn's commits under `turn.commit`, with
/// the turn's [`TurnWrite::Terminal`], which drops its phase row; a session
/// command's under `session.command`, settling the command's rows. It
/// publishes revision `expected_head + 1` and moves the session head to it,
/// with everything the session store writes beside a head (history nodes,
/// checkpoint, receipt, settlements); a session with no head is at revision
/// 0. The head compare-and-set makes a repeated commit refuse rather than
/// publish twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCommitWrite {
    /// The session.
    pub session: SessionId,
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
///
/// A request for a turn no run has opened yet, whose input is still queued
/// session mail, withdraws that input instead, in the same transaction
/// (FIG-5262). The mail row decides a race with the session's admission:
/// the admission binds only an open, unbound row, and the withdraw changes
/// only one, so exactly one of them takes it. A withdrawn input never runs;
/// an admitted one is the open run this request then cancels.
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
    /// No run had opened yet: the turn's input was still queued, open and
    /// unbound session mail, and the request withdrew it. The input never
    /// runs, and the session was woken.
    Withdrawn {
        /// The withdrawn input.
        input: InputId,
    },
    /// The turn is not the session's unfinished turn; nothing was written.
    AlreadyEnded,
}
