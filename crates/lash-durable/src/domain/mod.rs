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
pub mod prompts;
pub mod run_records;
pub mod session_close;
pub mod session_mail;
pub mod snapshots;
pub mod turns;
pub mod waits;

pub use keys::{CellId, ExecKey, Ordinal, OwnerKey, RunSeq, ScopeKey, StoredKeyError};
pub use park_events::{ParkEventKind, ParkEventRow, ParkEventSeq, ParkEventWrite};
pub use processes::{
    CancelAnswer, CancelRequest, ProcessActorRow, ProcessStartRows, ProcessWrite, RedriveAnswer,
    RedriveRequest,
};
pub use prompts::{ModelCallId, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite};
pub use run_records::{AdmittedId, RunRecordKind, RunRecordRow, RunRecordWrite};
pub use session_close::{SessionCloseRow, SessionCloseStep, SessionCloseWrite};
pub use session_mail::{
    MailBatch, MailBatchKind, MailInput, SESSION_ACTOR_FORMATS, SessionMailWrite, SessionMailbox,
    queued_input_run,
};
pub use snapshots::{SnapshotRev, SnapshotRow, SnapshotWrite};
pub use turns::{
    ModelPin, RunValuesWrite, SessionCommitWrite, TurnCancelAnswer, TurnCancelRequest, TurnEnd,
    TurnNamespace, TurnNamespaceWrite, TurnRow, TurnWrite, UnfinishedPhase,
};
pub use waits::{
    CANCEL_MAIL, ResolveAnswer, TIMER_DIGEST, WAIT_ROW_FORMAT_VERSION, WaitId, WaitKind,
    WaitLifecycle, WaitPurpose, WaitPurposeColumns, WaitResolution, WaitRow, WaitState, WaitWrite,
};

use crate::error::DurableError;
use crate::ids::ActorKey;
use lash_sansio::{BatchId, ProcessId, SessionId, ToolCallId, TurnId};

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
    /// L3s: the session's admission of its mail.
    SessionMail(SessionMailWrite),
    /// P2 (FIG-5256): a model call's prompt snapshot root and its shared
    /// text, and the explicit retention that releases them.
    Prompt(PromptWrite),
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
    /// wins, a stronger mode escalates) plus a control wake. A turn no run
    /// opened yet, whose input is still queued session mail, has that input
    /// withdrawn instead, with a wake (FIG-5262).
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
    /// Session mail an admission named is no longer open and unbound: a
    /// producer cancelled it, or another admission took it.
    #[error("session {session} mail {item} is no longer open")]
    SessionMailMoved {
        /// The session.
        session: SessionId,
        /// The input or batch.
        item: String,
    },
    /// The session refused mail its owner sent it: the session is closing
    /// or gone, or the mail's source key names another submission.
    #[error("session {session} refused its owner's mail: {reason}")]
    SessionMailRefused {
        /// The session.
        session: SessionId,
        /// The store's refusal.
        reason: String,
    },
    /// A process start's prepared registration was refused by the
    /// registrar: its starter or its lifetime's scope closed, or its
    /// consuming call was abandoned, since it was staged.
    #[error("process {process} could not be registered: {reason}")]
    ProcessStartRefused {
        /// The process the start minted.
        process: ProcessId,
        /// The registrar's refusal.
        reason: String,
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
    /// A session command the head commit settles is no longer open: a host
    /// withdrew it, or another commit settled it.
    #[error("session {session} command {batch} is no longer open")]
    SessionCommandWithdrawn {
        /// The session.
        session: SessionId,
        /// The command's batch.
        batch: BatchId,
    },
    /// The ancestor a session command's append requires has left the
    /// session's active path.
    #[error("session {session} append requires {required}, which left the active path")]
    AppendAncestorNotActive {
        /// The session.
        session: SessionId,
        /// The history node the append requires.
        required: lash_sansio::NodeId,
    },
    /// The session store refused a head commit's content: it breaks one of
    /// the store's own rules (a node id the session already holds, a reused
    /// identity, a budget), so the identical commit is refused alike on every
    /// pass and under any deployment
    /// ([`StoreError::refuses_request_content`](lash_core_store::store::StoreError::refuses_request_content)).
    #[error("session {session} refused the head commit: {reason}")]
    SessionCommitRefused {
        /// The session.
        session: SessionId,
        /// The code the store's refusal is carried under past the store
        /// (`StoreError::runtime_code`): whether retrying the commit can
        /// succeed is read from it, never from `reason`.
        code: lash_core_store::runtime_error::RuntimeErrorCode,
        /// The typed cause beside `code`, when the refusal has one.
        cause: Option<lash_core_store::runtime_error::RuntimeErrorCause>,
        /// The store's refusal, as it reads.
        reason: String,
    },
    /// The session store refused a head commit for the deployment or the
    /// state it holds, not for the commit's content: a writer format past
    /// the fleet record's range, a fence, a session state version this build
    /// does not read, unreadable rows, a session gone (FIG-5398). Every
    /// commit is refused alike until an operator's finalize, rollback or
    /// repair, so the commit's input is kept for the pass after it.
    #[error("session {session} cannot take a head commit: {reason}")]
    SessionCommitBlocked {
        /// The session.
        session: SessionId,
        /// The code the store's refusal is carried under past the store
        /// (`StoreError::runtime_code`).
        code: lash_core_store::runtime_error::RuntimeErrorCode,
        /// The typed cause beside `code`, when the refusal has one.
        cause: Option<lash_core_store::runtime_error::RuntimeErrorCause>,
        /// The store's refusal, as it reads.
        reason: String,
    },
    /// A session-close step is not the one after the stored step.
    #[error("session {session} close step {step:?} does not follow {done:?}")]
    SessionCloseOutOfOrder {
        /// The session.
        session: SessionId,
        /// The step the commit recorded.
        step: SessionCloseStep,
        /// The last step stored.
        done: Option<SessionCloseStep>,
    },
    /// A session-close step names a session whose close never began.
    #[error("session {session} is not closing")]
    SessionNotClosing {
        /// The session.
        session: SessionId,
    },
    /// A prompt record named a call that already has its snapshot.
    #[error("{} in session {} is already admitted", call.call, call.session)]
    PromptCallRecorded {
        /// The call.
        call: PromptCallKey,
    },
}

impl DomainRefusal {
    /// The store's refusal of a head commit's content, as the runtime error
    /// it is carried as past the store; `None` for every other refusal, a
    /// [`Self::SessionCommitBlocked`] among them.
    #[must_use]
    pub fn session_commit_refusal(&self) -> Option<lash_core_store::runtime_error::RuntimeError> {
        let Self::SessionCommitRefused {
            code,
            cause,
            reason,
            ..
        } = self
        else {
            return None;
        };
        let refusal = lash_core_store::runtime_error::RuntimeError::new(code.clone(), reason);
        Some(match cause {
            Some(cause) => refusal.with_cause(cause.clone()),
            None => refusal,
        })
    }
}

impl DurableError {
    /// The session store's refusal `error` of `session`'s head commit, as the
    /// durable port reports it (FIG-5398): a moved head, a settled command
    /// and a stale append as their own refusals; a refusal of the commit's
    /// content as [`DomainRefusal::SessionCommitRefused`]; a fault of the
    /// substrate as the store failure a retry clears; and every other
    /// refusal, the deployment's or the stored state's, as
    /// [`DomainRefusal::SessionCommitBlocked`]. Both carry the code and cause
    /// the store carries the refusal with past the store.
    #[must_use]
    pub fn session_commit(session: SessionId, error: &lash_core_store::store::StoreError) -> Self {
        use lash_core_store::store::StoreError;
        let refusal = match error {
            StoreError::HeadRevisionConflict { expected, actual } => DomainRefusal::HeadMoved {
                session,
                expected: *expected,
                found: Some(*actual),
            },
            StoreError::SessionCommandWithdrawn { batch_id, .. } => {
                DomainRefusal::SessionCommandWithdrawn {
                    session,
                    batch: batch_id.clone(),
                }
            }
            StoreError::AppendAncestorNotActive { required_node_id } => {
                DomainRefusal::AppendAncestorNotActive {
                    session,
                    required: required_node_id.clone(),
                }
            }
            error if error.is_transient() => {
                return Self::Store(crate::error::StoreFailure {
                    kind: if matches!(error, StoreError::Contended) {
                        crate::error::StoreFailureKind::Contended
                    } else {
                        crate::error::StoreFailureKind::Unavailable
                    },
                    message: error.to_string(),
                });
            }
            error if error.refuses_request_content() => DomainRefusal::SessionCommitRefused {
                session,
                code: error.runtime_code(),
                cause: error.runtime_cause(),
                reason: error.to_string(),
            },
            error => DomainRefusal::SessionCommitBlocked {
                session,
                code: error.runtime_code(),
                cause: error.runtime_cause(),
                reason: error.to_string(),
            },
        };
        Self::Domain(refusal)
    }
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

    /// FIG-5301: the namespace rows of `session`'s unfinished `run`, by
    /// plugin.
    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<TurnNamespace>, DurableError>;

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

    /// FIG-5787: the latest snapshot of each cell of `session`'s `run`, by
    /// execution key: what an operator's survey reads of a turn stopped in
    /// a cell.
    async fn cell_snapshots(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<SnapshotRow>, DurableError>;

    /// L5: every pending wait `owner` owns.
    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError>;

    /// L5: one wait.
    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError>;

    /// L6: one process actor's row.
    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError>;

    /// L6: up to `limit` non-terminal processes, by id, in `scope`'s `Until`
    /// subtree: those whose lifetime is `Until` `scope` or one of its
    /// descendants, ended or not, and for a session those of its turn and
    /// session-operation scopes too. A parent's terminal does not mean this
    /// is empty: the subtree's ends are not a durable fact.
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

    /// L6b: the session's closing state, or its tombstone once closed.
    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError>;

    /// L6b: the session's scopes whose cascade still has children to mark,
    /// in the order they began ending.
    async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError>;

    /// L3s: the session's open mail.
    async fn session_mailbox(&self, session: &SessionId) -> Result<SessionMailbox, DurableError>;

    /// L6: up to `limit` park-feed entries after `after`, oldest first.
    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError>;

    /// P2: `call`'s prompt snapshot, while its root is retained.
    async fn prompt_snapshot(
        &self,
        call: &PromptCallKey,
    ) -> Result<Option<PromptSnapshotRow>, DurableError>;

    /// P2: the stored texts among `hashes`, in `hashes` order; a hash no
    /// root retains is absent.
    async fn prompt_texts(&self, hashes: &[String]) -> Result<Vec<PromptText>, DurableError>;
}
