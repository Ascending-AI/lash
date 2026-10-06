//! Turn rows (V0, then L3) and the turn-cancel mail (L3).
//!
//! A turn is a sequence of committed phases of the sans-io `TurnMachine`
//! (ADR 0132 §4). Its row names the phase, the checkpoint by digest, the
//! pinned model request and the terminal. At most one turn per session is
//! unfinished.

use crate::ids::{DurableInstant, Epoch};
use lash_sansio::{SessionId, TurnId};

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
    /// The pinned request, by digest.
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
    /// The bounded `TurnCheckpoint`, by digest.
    pub checkpoint_ref: Option<String>,
    /// The in-flight model call.
    pub model: Option<ModelPin>,
    /// The host's turn deadline, recorded at admission.
    pub turn_deadline: Option<DurableInstant>,
    /// The epoch of the commit that last wrote the row.
    pub written_epoch: Epoch,
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
    Advance {
        /// The session.
        session: SessionId,
        /// The run.
        run: TurnId,
        /// The new phase; never [`TurnPhase::Terminal`].
        phase: TurnPhase,
        /// The protocol iteration.
        iteration: u32,
        /// The checkpoint, by digest.
        checkpoint_ref: Option<String>,
        /// The in-flight model call, or `None` once it is done.
        model: Option<ModelPin>,
    },
    /// End the turn.
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

/// The turn's commit to its session (V0, then L3): the head
/// compare-and-set, `lash_runtime_turn_commits`, and pruning of the turn's
/// phase rows, in the `turn.commit` transaction. The head compare-and-set
/// makes a repeated commit idempotent.
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
    /// The commit plan (history, checkpoint components), encoded by its
    /// owner.
    pub commit_json: String,
}

/// A request to cancel a session's turn (L3): a mailbox row plus a
/// control wake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCancelRequest {
    /// The session.
    pub session: SessionId,
    /// The turn to cancel; `None` for whichever is unfinished.
    pub run: Option<TurnId>,
    /// The request (reason, affected inputs), encoded by its owner.
    pub request_json: String,
}

/// The answer to a [`TurnCancelRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnCancelAnswer {
    /// Recorded, and the session woken.
    Recorded,
    /// The named turn is already terminal; nothing was written.
    AlreadyEnded,
}
