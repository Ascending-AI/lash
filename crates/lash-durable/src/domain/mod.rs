//! Domain rows the runtime lanes add to the port's commits (I0, FIG-5194).
//!
//! L1's [`ActorTx`](crate::ActorTx) and [`MailTx`](crate::MailTx) carry only
//! acknowledgement, release, creation, append and wake. Every runtime lane
//! also writes rows of its own domain, and some write them conditionally from
//! outside the owner. This module is that extension, once: row types and the
//! write and read vocabulary, never SQL.
//!
//! # Contracts
//!
//! - **The fence stays first.** A store applies an owner commit's
//!   [`DomainWrite`]s after L1's fence statement, in the order they were
//!   recorded, then the acknowledgement, then the release, all in one
//!   transaction. A refused domain write ([`DomainRefusal`]) rolls the whole
//!   commit back, fence bump included: nothing is written.
//! - **Mail writes are conditional and answered.** A [`MailDomainWrite`] is
//!   applied in its place among the mailbox transaction's other writes; the
//!   commit's [`MailCommit::answers`](crate::MailCommit::answers) holds one
//!   [`MailAnswer`] per domain write, in order.
//! - **Owner reads are unfenced and safe.** Only the owner writes owner-state
//!   rows, so a [`DurableReads`] read after `begin` is current unless
//!   ownership was lost, which the next commit's fence reports.
//! - **Who fills what.** Each domain's row types, its SQL (one neutral
//!   statements file and one file per dialect) and its apply and read bodies
//!   belong to the lane named on its module. The dispatch from a variant to
//!   its domain file is I0's and stays as it is; a lane adds fields and
//!   variants to its own domain's types only.

mod keys;
pub mod park_events;
pub mod processes;
pub mod run_records;
pub mod session_close;
pub mod snapshots;
pub mod turns;
pub mod waits;

pub use keys::{CellId, ExecKey, Ordinal, OwnerKey, RunSeq, ScopeKey, StoredKeyError};
pub use park_events::{ParkEventKind, ParkEventRow, ParkEventSeq, ParkEventWrite};
pub use processes::{
    CancelAnswer, CancelRequest, PROCESS_FORMATS, ProcessActorRow, ProcessStartRows, ProcessWrite,
    RedriveAnswer, RedriveRequest, SIGNAL_MAIL,
};
pub use run_records::{AdmittedId, RunRecordKind, RunRecordRow, RunRecordWrite};
pub use session_close::{SessionCloseStep, SessionCloseWrite};
pub use snapshots::{SnapshotRev, SnapshotRow, SnapshotWrite};
pub use turns::{
    ModelPin, SessionCommitWrite, TurnCancelAnswer, TurnCancelRequest, TurnEnd, TurnPhase, TurnRow,
    TurnTerminal, TurnWrite,
};
pub use waits::{
    CANCEL_MAIL, KeyVersion, ResolveAnswer, TIMER_DIGEST, WaitId, WaitKind, WaitResolution,
    WaitRow, WaitState, WaitWrite,
};

use crate::error::DurableError;
use crate::ids::ActorKey;
use lash_sansio::{ProcessId, SessionId, ToolCallId, TurnId};

/// One owner-state write inside a fenced [`ActorTx`](crate::ActorTx) commit.
///
/// Applied in order, after the fence and before the acknowledgement and the
/// release. Each variant is applied by its domain's file in each dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DomainWrite {
    /// V0, then L3: the turn's phase row, checkpoint ref, model pin and
    /// terminal.
    Turn(TurnWrite),
    /// V0, then L3: the session head compare-and-set,
    /// `lash_runtime_turn_commits` and pruning of the turn's phase rows.
    SessionCommit(SessionCommitWrite),
    /// V0, then L4: run records (admit, `x_start`, `x_outcome`, decide,
    /// present, retry).
    RunRecord(RunRecordWrite),
    /// V0, then L7: a VM snapshot, compare-and-set on its revision.
    Snapshot(SnapshotWrite),
    /// L5: wait pinning, timeout, scope revocation and process-terminal
    /// resolution.
    Wait(WaitWrite),
    /// L6: the process actor and registry row, state revision, cancel,
    /// terminal and cascade cursor.
    Process(ProcessWrite),
    /// L6b: the session closing state's steps.
    SessionClose(SessionCloseWrite),
    /// L6: one entry of the operator's park feed.
    ParkEvent(ParkEventWrite),
}

/// One conditional non-owner write inside a [`MailTx`](crate::MailTx).
///
/// Each is answered in [`MailCommit::answers`](crate::MailCommit::answers),
/// and each wakes the actor it concerns when it changed something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailDomainWrite {
    /// L5: resolve a wait, conditional from `pending`; the first resolution
    /// wins. Lock order is the wait row, then the actor row.
    ResolveWait(WaitResolution),
    /// L6: request a process's cancel. The first request wins and keeps its
    /// timestamp; it wakes the process as a control wake, which readies even
    /// a parked actor.
    RequestProcessCancel(CancelRequest),
    /// L3: request a turn's cancel: its cancel-request row (first policy
    /// wins, a stronger mode escalates) plus a control wake.
    RequestTurnCancel(TurnCancelRequest),
    /// L6: redrive a parked actor: clear its park and its failed
    /// activations, record the redrive on the park feed, and control-wake
    /// it.
    Redrive(RedriveRequest),
}

/// The answer to one [`MailDomainWrite`], in the order they were recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailAnswer {
    /// The answer to [`MailDomainWrite::ResolveWait`].
    ResolveWait(ResolveAnswer),
    /// The answer to [`MailDomainWrite::RequestProcessCancel`].
    RequestProcessCancel(CancelAnswer),
    /// The answer to [`MailDomainWrite::RequestTurnCancel`].
    RequestTurnCancel(TurnCancelAnswer),
    /// The answer to [`MailDomainWrite::Redrive`].
    Redrive(RedriveAnswer),
}

/// A conditional domain write that found the rows otherwise than it
/// required. The whole commit rolled back: nothing it carried was written.
///
/// One variant per conditional write; a lane that adds a conditional write
/// adds its refusal here.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DomainRefusal {
    /// A snapshot write's expected revision was not the stored one.
    #[error("snapshot {exec} is at revision {found:?}, not the expected {expected:?}")]
    SnapshotRevConflict {
        /// The execution.
        exec: ExecKey,
        /// The revision the write expected to replace.
        expected: Option<SnapshotRev>,
        /// The revision stored.
        found: Option<SnapshotRev>,
    },
    /// A run record's ordinal is already taken: the second fence.
    #[error("run record {owner} run {run:?} ordinal {ordinal:?} is already taken")]
    RunOrdinalTaken {
        /// The owner.
        owner: OwnerKey,
        /// The run.
        run: RunSeq,
        /// The ordinal.
        ordinal: Ordinal,
    },
    /// A run record would leave a gap: its run has no record at the
    /// ordinal before it. Ordinals are the owner's, in sequence.
    #[error("run record {owner} run {run:?} ordinal {ordinal:?} follows no record")]
    RunOrdinalGap {
        /// The owner.
        owner: OwnerKey,
        /// The run.
        run: RunSeq,
        /// The ordinal refused.
        ordinal: Ordinal,
    },
    /// An admitted call already has its outcome.
    #[error("call {call} of {owner} run {run:?} already has an outcome")]
    OutcomeExists {
        /// The owner.
        owner: OwnerKey,
        /// The run.
        run: RunSeq,
        /// The call.
        call: ToolCallId,
    },
    /// A turn write named a turn that is not the session's unfinished one.
    #[error("turn {run} of session {session} is not unfinished")]
    TurnNotOpen {
        /// The session.
        session: SessionId,
        /// The turn.
        run: TurnId,
    },
    /// The session already has an unfinished turn.
    #[error("session {session} already has an unfinished turn")]
    OpenTurnExists {
        /// The session.
        session: SessionId,
    },
    /// A process transition's expected state revision was not the stored
    /// one, or the process is gone or terminal.
    #[error("process {process} is at state revision {found:?}, not the expected {expected}")]
    ProcessRevConflict {
        /// The process.
        process: ProcessId,
        /// The revision the transition expected to replace.
        expected: u64,
        /// The revision stored; `None` when the process is gone or terminal.
        found: Option<u64>,
    },
    /// The session head moved past the revision the commit expected.
    #[error("session {session} head is at {found:?}, not the expected {expected}")]
    HeadMoved {
        /// The session.
        session: SessionId,
        /// The head revision the commit expected.
        expected: u64,
        /// The head revision stored.
        found: Option<u64>,
    },
    /// The session store refused the turn's head commit for a reason other
    /// than a moved head: the session is gone, or the commit breaks one of
    /// the store's own rules.
    #[error("session {session} refused the turn's head commit: {reason}")]
    SessionCommitRefused {
        /// The session.
        session: SessionId,
        /// The store's refusal.
        reason: String,
    },
}

/// The owner's and operators' reads of domain rows, unfenced.
///
/// [`DurableStore`](crate::DurableStore) requires it, so every store, and
/// every decorator over one, answers all of them. No method has a default
/// body.
#[async_trait::async_trait]
pub trait DurableReads: Send + Sync {
    /// V0, then L3: the session's unfinished turn, if any.
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError>;

    /// L3: how `run` of `session` ended, once it ended.
    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<TurnEnd>, DurableError>;

    /// V0, then L4: every run record of `owner`, ordered by run and ordinal.
    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError>;

    /// V0, then L7: the latest snapshot of `exec`.
    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError>;

    /// L5: every pending wait `owner` owns.
    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError>;

    /// L5: one wait.
    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError>;

    /// L6: one process actor's row.
    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError>;

    /// L6: up to `limit` non-terminal processes whose lifetime is `Until`
    /// `scope` or one of its descendants. A parent's terminal does not mean
    /// this is empty: the subtree's ends are not a durable fact.
    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError>;

    /// L6: up to `limit` live `Until(scope)` children whose cancel was not
    /// yet requested, ordered by process id, after `after`: the next batch
    /// of a scope's cascade.
    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError>;

    /// L6: up to `limit` park-feed entries after `after`, oldest first.
    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError>;
}
