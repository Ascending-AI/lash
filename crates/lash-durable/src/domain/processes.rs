//! Process actor rows (L6): the process actor and registry row, its state
//! revision, cancel, terminal and cascade cursor (ADR 0132 §10, §11).
//!
//! The registry row is the process's one row: its outcome, its lifetime
//! scope and its first cancel request are the registry's, and the actor
//! columns beside them (state revision, the driver's pending action, the
//! cascade cursor) are the process actor's. The engine's state is the
//! process's snapshot, `p/<pid>` in the snapshots domain, and moves in the
//! same commit as the state revision.

use crate::ids::{DurableInstant, Epoch};
use lash_sansio::{CancelOrigin, ProcessId};

use super::keys::ScopeKey;

/// The mail kind of a signal sent to a process: its body is the signal,
/// encoded by its owner, and it reaches `advance` as one event.
pub const SIGNAL_MAIL: &str = "signal";

/// One process actor's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessActorRow {
    /// The process.
    pub process: ProcessId,
    /// Its engine state's revision; advances with every committed
    /// transition. Zero before the first.
    pub state_rev: u64,
    /// The process driver's state, encoded by its owner: what the last
    /// committed transition left in flight. `None` before the first.
    pub driver_json: Option<String>,
    /// When its cancel was first requested.
    pub cancel_requested_at: Option<DurableInstant>,
    /// Whether it is terminal.
    pub terminal: bool,
    /// The terminal's cascade cursor while `Until` children remain to be
    /// marked: the last child marked, or empty before the first batch.
    pub cascade_cursor: Option<String>,
    /// The epoch of the commit that last wrote it.
    pub written_epoch: Option<Epoch>,
    /// The last event of its log its owners handed to their hosts' sinks:
    /// where publication resumes after a takeover. Zero before the first.
    pub published_event_sequence: u64,
}

/// The rows a process start writes: its registry row, its observers and
/// its actor, ready, in the transaction that records the outcome of the
/// call that started it (ADR 0132 §5). A start whose key a process already
/// holds writes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessStartRows {
    /// The process.
    pub process: ProcessId,
    /// The prepared registration (its observers, the minted process id and
    /// the instant it was prepared at), encoded by its owner.
    pub registration_json: String,
}

/// A process write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessWrite {
    /// Register a process: its registry row, its observers and its actor,
    /// ready. Refused with
    /// [`DomainRefusal::ProcessStartRefused`](super::DomainRefusal::ProcessStartRefused)
    /// when the registrar refuses the start.
    Register(ProcessStartRows),
    /// Send a signal, encoded by its owner, to `process`: its event is
    /// appended exactly once under the signal's identity, and its mail and
    /// the process's wake ride the same commit. A process that is unknown
    /// or already terminal takes nothing: the signal reached no live
    /// process, as one sent just before the process ended.
    Signal {
        /// The process.
        process: ProcessId,
        /// The signal.
        signal_json: String,
    },
    /// Commit one engine transition: the state revision moves from
    /// `expected_rev` to `expected_rev + 1` and the driver state is
    /// replaced. Refused with
    /// [`DomainRefusal::ProcessRevConflict`](super::DomainRefusal::ProcessRevConflict)
    /// when the stored revision is not `expected_rev`.
    Advance {
        /// The process.
        process: ProcessId,
        /// The revision replaced.
        expected_rev: u64,
        /// The driver state after the transition.
        driver_json: String,
    },
    /// Record that the owner's host sinks were handed `process`'s log
    /// through sequence `through`. The mark only moves forward: an older
    /// one leaves it as it is.
    Published {
        /// The process.
        process: ProcessId,
        /// The last event handed to the sinks.
        through: u64,
    },
    /// Append one process event, encoded by its owner, exactly once: the
    /// replay key makes a repeat of the same commit a no-op.
    Emit {
        /// The process.
        process: ProcessId,
        /// The event's type.
        event_type: String,
        /// Its payload.
        payload_json: String,
        /// Its replay key.
        replay_key: String,
        /// Whether the wake its type declares is withheld: a parked call's
        /// announcement of its own wait wakes nobody, while the event is
        /// journaled as any other append of its type is.
        wake_suppressed: bool,
    },
    /// End the process with its terminal, encoded by its owner. A process
    /// already terminal keeps its first terminal. The cascade over its
    /// `Until` children starts: its cursor is set, empty.
    Terminal {
        /// The process.
        process: ProcessId,
        /// The terminal outcome.
        outcome_json: String,
    },
    /// Mark `children` (live `Until(scope)` children whose cancel was not
    /// yet requested) for cancel with `origin`: each records its first
    /// cancel request, gets a cancel mail and a control wake. When `scope`
    /// is the committing process, its cursor moves to `cursor`; `None`
    /// once the cascade is done.
    CascadeBatch {
        /// The ending scope.
        scope: ScopeKey,
        /// The children this batch marks.
        children: Vec<ProcessId>,
        /// Why they are cancelled.
        origin: CancelOrigin,
        /// Who asked, for the children's cancel requests.
        requester: String,
        /// The cursor after this batch.
        cursor: Option<String>,
    },
    /// Close `scope`: its closure fact, the registry's `parent_end_plans`
    /// row, commits with the transaction that ended it, and from then on
    /// every registration under the scope, or inside it, is refused. On
    /// PostgreSQL the write takes the scope's advisory lock first, which a
    /// registration deciding the same scope also holds, so the start either
    /// commits before the closure or reads it. The scope's `Until` children
    /// are read after that, in the transaction: when a turn scope still has
    /// one unmarked once this transaction's batch is applied, the turn scope
    /// is recorded as ending, so the session marks the rest. Closing a
    /// closed scope keeps its first fact.
    ScopeClosed {
        /// The closed scope.
        scope: ScopeKey,
    },
}

/// A request to cancel a process, from outside its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRequest {
    /// The process.
    pub process: ProcessId,
    /// Why.
    pub origin: CancelOrigin,
    /// Who asked.
    pub requester: String,
}

/// The answer to a [`CancelRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelAnswer {
    /// The first request: recorded at `at`, a cancel mail appended and the
    /// process control-woken.
    Requested {
        /// When.
        at: DurableInstant,
    },
    /// An earlier request stands, with its own timestamp; the process was
    /// control-woken again.
    AlreadyRequested {
        /// The earlier request's time.
        at: DurableInstant,
    },
    /// The process is already terminal.
    AlreadyEnded,
}

/// A request to redrive a parked actor, from an operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedriveRequest {
    /// The actor.
    pub actor: crate::ids::ActorKey,
    /// Who asked.
    pub requester: String,
}

/// The answer to a [`RedriveRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedriveAnswer {
    /// It was parked: its park and its failed activations are cleared, the
    /// feed records the redrive, and it is ready.
    Redriven,
    /// It was not parked; nothing was written.
    NotParked,
}
