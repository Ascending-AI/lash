//! The logical run's durable record (FIG-3600 S7, FIG-3607 contract 2 and
//! item 8): one row per `(session, run)` holding the run's terminal
//! evidence, beside the bindings of the accepted inputs the run drove.
//!
//! A logical run is what a host names a turn by. Its physical turns (a frame
//! switch's follow-on, an S4 follow-on, a redrive) all derive from it, and
//! exactly one of them ends it. The evidence of that end is run-addressed and
//! written in the transaction that makes it true: the head commit of the
//! run's final physical turn, or the run verb, close or lost-run end that
//! ended it without one. That transaction also releases every row still
//! bound to the run, so no row stays admitted to a run with terminal
//! evidence (FIG-3927). Once written it is never replaced: a second, different
//! terminal is refused with [`StoreError::RunAlreadyTerminal`] and the first
//! stands (ADR 0105 law L-S6), while rewriting the same terminal is a no-op.
//!
//! The evidence outlives the session's ingress rows (vacuum prunes those) and
//! lasts until the session is deleted. After deletion the factory still
//! answers every run of the session from its retained deletion intent
//! ([`RunTerminalCause::SessionDeleted`]).
//!
//! [`TurnCommitId`] lives here, below the engine contract, because the stores
//! write it; `lash_core::engine` re-exports it unchanged.

use serde::{Deserialize, Serialize};

use super::StoreError;
use super::control_intent::ControlIntentId;
use crate::{BatchId, InputId, SessionId, TurnId};
use lash_sansio::{TurnFinish, TurnOutcome, TurnStop};

/// A turn commit's identity: the logical run and the physical ordinal of
/// the attempt that commits it. Derived, never minted: the ordinal is the
/// physical turn's position within its run
/// ([`PhysicalTurn::derive_turn_id`](super::PhysicalTurn::derive_turn_id)).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnCommitId {
    run: TurnId,
    ordinal: u32,
}

impl TurnCommitId {
    pub fn new(run: TurnId, ordinal: u32) -> Self {
        Self { run, ordinal }
    }

    pub fn run(&self) -> &TurnId {
        &self.run
    }

    pub fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// The commit id of physical turn `turn` of `run`: `Some` exactly when
    /// `turn` is one of `run`'s physical turns.
    #[must_use]
    pub fn of_physical_turn(run: &TurnId, turn: &TurnId) -> Option<Self> {
        let ordinal = super::PhysicalTurn::physical_ordinal_of(run, turn)?;
        Some(Self::new(run.clone(), u32::try_from(ordinal).ok()?))
    }
}

/// How a logical run ended, as a host reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTerminalKind {
    Answered,
    Failed,
    Cancelled,
}

impl RunTerminalKind {
    /// The stored code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The kind a stored code names.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "answered" => Some(Self::Answered),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// The kind a physical turn's stop names: none is an answer, a cancelled
    /// stop is a cancellation, and every other stop is a failure.
    #[must_use]
    pub fn of_stop(stop: Option<&TurnStop>) -> Self {
        match stop {
            None => Self::Answered,
            Some(TurnStop::Cancelled { .. }) => Self::Cancelled,
            Some(_) => Self::Failed,
        }
    }
}

/// The outcome a run's final physical turn committed with: it finished, it
/// switched the agent frame (its follow-on task is the session's next turn,
/// ADR 0101 §3), or it stopped. Encoded as the matching [`TurnOutcome`]
/// variant; a frame switch's seed nodes are the new frame's committed
/// history, not its run's answer, so its record leaves them out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunCommittedOutcome {
    Finished(TurnFinish),
    AgentFrameSwitch {
        frame_key: lash_sansio::FrameKey,
        task: String,
    },
    Stopped(TurnStop),
}

impl RunCommittedOutcome {
    /// The committed outcome `outcome` ends a run with.
    #[must_use]
    pub fn of_turn_outcome(outcome: &TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Finished(finish) => Self::Finished(finish.clone()),
            TurnOutcome::AgentFrameSwitch {
                frame_key, task, ..
            } => Self::AgentFrameSwitch {
                frame_key: frame_key.clone(),
                task: task.clone(),
            },
            TurnOutcome::Stopped(stop) => Self::Stopped(stop.clone()),
        }
    }

    /// Why the turn stopped; `None` when it finished or switched frames.
    #[must_use]
    pub fn stop(&self) -> Option<&TurnStop> {
        match self {
            Self::Finished(_) | Self::AgentFrameSwitch { .. } => None,
            Self::Stopped(stop) => Some(stop),
        }
    }
}

impl From<RunCommittedOutcome> for TurnOutcome {
    fn from(outcome: RunCommittedOutcome) -> Self {
        match outcome {
            RunCommittedOutcome::Finished(finish) => Self::Finished(finish),
            RunCommittedOutcome::AgentFrameSwitch { frame_key, task } => Self::AgentFrameSwitch {
                frame_key,
                task,
                initial_nodes: Vec::new(),
            },
            RunCommittedOutcome::Stopped(stop) => Self::Stopped(stop),
        }
    }
}

/// Why a run is terminal: the transaction that wrote its evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum RunTerminalCause {
    /// The run's final physical turn committed (the head-commit
    /// transaction), with `outcome`: what the run answers every input it
    /// took, read from this row alone (FIG-4345).
    Committed {
        commit: TurnCommitId,
        turn: TurnId,
        outcome: RunCommittedOutcome,
    },
    /// The session actor honoured a cancel request before the run's turn
    /// committed (ADR 0132 §3): the run ends with the request's evidence and
    /// the session head does not move.
    Cancelled {
        evidence: lash_sansio::TurnCancellationEvidence,
    },
    /// An operator cancelled the parked run (its control intent).
    OperatorCancelled { intent: ControlIntentId },
    /// An operator forked the parked run; its held inputs now shift
    /// `new_run`.
    Forked {
        intent: ControlIntentId,
        new_run: Option<TurnId>,
    },
    /// The session was deleted (its `CloseSession` intent).
    SessionDeleted { intent: ControlIntentId },
    /// The run's execution ended with a typed refusal no retry could change (a
    /// superseded commit, a finalize refusal): the run's own end, written
    /// before the engine records its outcome (FIG-4018). It keeps the
    /// refusal, which is the answer of every input the run took, with its
    /// structured cause: a session-retirement refusal answers as one.
    Refused {
        refusal: crate::runtime_error::RecordedRefusal,
    },
    /// A command run applied the session's open command run and admitted
    /// no turn (ADR 0101 §4, FIG-4202). Its end, written once the command
    /// lane is empty, arms its scope close as every run's end does, so the
    /// run's journal is retired like any other.
    CommandsApplied,
}

impl RunTerminalCause {
    /// The cause's one stored form: `session_runs.terminal_cause_json`. Its
    /// `terminal_kind` column is [`Self::kind`], checked against it by the
    /// DDL.
    ///
    /// # Errors
    ///
    /// [`StoreError::RecordEncodingFailed`] when it does not encode.
    pub fn to_stored(&self) -> Result<String, StoreError> {
        serde_json::to_string(self).map_err(|error| StoreError::RecordEncodingFailed {
            record_kind: "RunTerminal".to_string(),
            message: error.to_string(),
        })
    }

    /// The cause `stored` holds, decoded by the codec that wrote it.
    ///
    /// # Errors
    ///
    /// [`StoreError::StoredDataCorrupt`] when it does not decode.
    pub fn from_stored(stored: &str) -> Result<Self, StoreError> {
        serde_json::from_str(stored).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "RunTerminal",
            message: format!("run terminal cause: {error}"),
        })
    }

    /// The kind this cause answers.
    #[must_use]
    pub fn kind(&self) -> RunTerminalKind {
        match self {
            Self::Committed { outcome, .. } => RunTerminalKind::of_stop(outcome.stop()),
            Self::Cancelled { .. }
            | Self::OperatorCancelled { .. }
            | Self::Forked { .. }
            | Self::SessionDeleted { .. } => RunTerminalKind::Cancelled,
            Self::Refused { .. } => RunTerminalKind::Failed,
            Self::CommandsApplied => RunTerminalKind::Answered,
        }
    }
}

/// A logical run's terminal evidence, as the store answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTerminal {
    pub session_id: SessionId,
    pub run: TurnId,
    pub cause: RunTerminalCause,
    /// The head revision of the terminal transaction; `None` when the
    /// transaction moved no head (a settlement, an intent) or the answer comes
    /// from a deletion tombstone.
    pub head_revision: Option<u64>,
    pub at_ms: u64,
}

impl RunTerminal {
    /// The kind this terminal answers: its cause's, never a second fact.
    #[must_use]
    pub fn kind(&self) -> RunTerminalKind {
        self.cause.kind()
    }

    /// The commit that ended the run, when a head commit did.
    #[must_use]
    pub fn commit(&self) -> Option<&TurnCommitId> {
        match &self.cause {
            RunTerminalCause::Committed { commit, .. } => Some(commit),
            _ => None,
        }
    }

    /// Whether `other` is this same terminal: one cause. The
    /// instant and head revision are the writer's, never part of identity,
    /// so a retried writer's rewrite is recognized as the same terminal.
    #[must_use]
    pub fn same_terminal(&self, other: &Self) -> bool {
        self.session_id == other.session_id && self.run == other.run && self.cause == other.cause
    }
}

/// What a head commit writes for its run: present exactly on the commit of
/// a run's final physical turn. Replaces P0's `TurnTerminalEvidence`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTerminalWrite {
    pub run: TurnId,
    pub commit: TurnCommitId,
    /// The physical turn that reached the terminal.
    pub turn: TurnId,
    /// The outcome the turn committed with.
    pub outcome: RunCommittedOutcome,
}

impl RunTerminalWrite {
    /// The kind this write answers.
    #[must_use]
    pub fn kind(&self) -> RunTerminalKind {
        RunTerminalKind::of_stop(self.outcome.stop())
    }

    /// The cause this write records.
    #[must_use]
    pub fn cause(&self) -> RunTerminalCause {
        RunTerminalCause::Committed {
            commit: self.commit.clone(),
            turn: self.turn.clone(),
            outcome: self.outcome.clone(),
        }
    }

    /// The evidence this write becomes in session `session_id`'s transaction
    /// at `head_revision`.
    #[must_use]
    pub fn into_terminal(
        self,
        session_id: SessionId,
        head_revision: u64,
        at_ms: u64,
    ) -> RunTerminal {
        RunTerminal {
            session_id,
            cause: self.cause(),
            run: self.run,
            head_revision: Some(head_revision),
            at_ms,
        }
    }
}

/// Decide one terminal write against the stored evidence of its run.
///
/// No stored evidence: write. The same terminal: a retried writer, answer
/// without writing. Any other terminal: refuse, and the stored one stands
/// (ADR 0105 law L-S6).
pub fn decide_run_terminal_write(
    stored: Option<&RunTerminal>,
    write: &RunTerminal,
) -> Result<RunTerminalWriteDecision, StoreError> {
    match stored {
        None => Ok(RunTerminalWriteDecision::Write),
        Some(stored) if stored.same_terminal(write) => Ok(RunTerminalWriteDecision::AlreadyWritten),
        Some(stored) => Err(StoreError::RunAlreadyTerminal {
            session_id: write.session_id.clone(),
            run: write.run.clone(),
            by: Box::new(stored.cause.clone()),
        }),
    }
}

/// What a backend does for one terminal write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunTerminalWriteDecision {
    Write,
    AlreadyWritten,
}

/// What [`RunStore::end_refused_run`] or [`RunStore::end_command_run`]
/// did (FIG-4018, FIG-4200, FIG-4202).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEndOutcome {
    /// The write ended the run.
    Ended(RunTerminal),
    /// The run already had terminal evidence, which stands: a replay of the
    /// run that wrote the end, or whatever else ended the run first. Nothing
    /// was written.
    AlreadyEnded(RunTerminal),
    /// The store holds no row for the run. Nothing was written.
    Unknown,
}

impl RunEndOutcome {
    /// The run's terminal evidence, when the run has ended.
    #[must_use]
    pub fn terminal(&self) -> Option<&RunTerminal> {
        match self {
            Self::Ended(terminal) | Self::AlreadyEnded(terminal) => Some(terminal),
            Self::Unknown => None,
        }
    }
}

/// The stored form of a run's terminal: the columns a backend writes.
/// `kind` is a projection of the cause for SQL to select on; no reader
/// decodes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRunTerminal {
    pub kind: &'static str,
    pub cause_json: String,
    pub head_revision: Option<u64>,
    pub at_ms: u64,
}

impl RunTerminal {
    /// The columns a backend stores.
    pub fn to_stored(&self) -> Result<StoredRunTerminal, StoreError> {
        Ok(StoredRunTerminal {
            kind: self.kind().as_str(),
            cause_json: self.cause.to_stored()?,
            head_revision: self.head_revision,
            at_ms: self.at_ms,
        })
    }

    /// Decode the columns a backend stored for `run` of `session_id`.
    pub fn from_stored(
        session_id: SessionId,
        run: TurnId,
        cause_json: &str,
        head_revision: Option<u64>,
        at_ms: u64,
    ) -> Result<Self, StoreError> {
        let cause = RunTerminalCause::from_stored(cause_json)?;
        Ok(Self {
            session_id,
            run,
            cause,
            head_revision,
            at_ms,
        })
    }
}

/// The per-session store half of logical runs: the terminal read, and the
/// bindings of accepted inputs to the runs that execute them.
#[async_trait::async_trait]
pub trait RunStore: Send + Sync {
    /// The session's one admitted run without terminal evidence, and the
    /// head its admission recorded, if there is one. Admission resumes it
    /// before anything else (FIG-3927).
    async fn unfinished_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<UnfinishedRun>, StoreError>;

    /// Admit the rows a running run's checkpoint delivers, in one
    /// transaction (FIG-3927).
    ///
    /// Rows already bound to `(request.run, request.step)` are returned
    /// exactly, in `enqueue_seq` order, and nothing else is taken: a
    /// re-execution of the checkpoint step reads its own admission back. A
    /// first execution applies the follow-on block (except the follow-on's
    /// own checkpoint), composes the addressed active-turn inputs the
    /// checkpoint's boundary admits and the queued work the boundary admits,
    /// binds them to the run with `admitted_by = request.step`, and delivers
    /// their obligations.
    async fn admit_at_checkpoint(
        &self,
        request: &CheckpointAdmissionRequest,
    ) -> Result<CheckpointAdmission, StoreError>;

    /// The terminal evidence of `run` in `session_id`, if it has any.
    async fn run_terminal(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Option<RunTerminal>, StoreError>;

    /// End `run` of `session_id`, whose execution met `refusal`, a typed
    /// refusal no retry can change, with [`RunTerminalCause::Refused`]
    /// (FIG-4018).
    ///
    /// One transaction, as for a lost run: the head's owed follow-on is
    /// cleared, the run's own inputs are cancelled and its batches removed,
    /// and the terminal write releases whatever else it held and arms its
    /// scope close. The session's next admission then executes a new run.
    ///
    /// A run that already has terminal evidence is left as it is and
    /// answers [`RunEndOutcome::AlreadyEnded`], so a replay of the run that
    /// wrote the end writes nothing more. A run with no row answers
    /// [`RunEndOutcome::Unknown`].
    async fn end_refused_run(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        refusal: &crate::RuntimeError,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError>;

    /// End command run `run` of `session_id`, whose execution applied the
    /// session's command lane until it was empty, with
    /// [`RunTerminalCause::CommandsApplied`] (FIG-4202).
    ///
    /// A command run binds no rows, so the store holds no row for it until
    /// this write: the transaction opens the run's row and writes its
    /// terminal, which arms its scope close. A run that already has terminal
    /// evidence answers [`RunEndOutcome::AlreadyEnded`], so a replay of the run
    /// that wrote the end writes nothing more.
    async fn end_command_run(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError>;

    /// The run that took accepted input `input`: the run its admission bound
    /// it to, or for a checkpoint delivery the run whose commit applied it.
    /// `None` while it is pending or while a checkpoint delivery is in flight.
    async fn run_of_input(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError>;

    /// The run input `input` is bound to, if any: admission executes a bound
    /// input under that run (a fork binds held inputs to its new run).
    async fn run_binding(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError>;

    /// The turn scopes named by inputs bound to `run`. A joined input's
    /// source key names its turn scope; an unkeyed input uses its input id.
    /// These bindings outlive input settlement until session deletion.
    async fn bound_turn_scopes(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<TurnId>, StoreError>;

    /// Bind each of `inputs` to `run`, set-if-absent, and open `run`'s
    /// record if it has none. The run's admission binds the rows it
    /// admitted. A binding to another run is refused and nothing is written.
    async fn bind_run_inputs(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError>;
}

/// The turn-lane row a run's admission is headed by.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "head", content = "id", rename_all = "snake_case")]
pub enum AdmittedHead {
    /// An accepted next-turn input.
    Input(InputId),
    /// A ready queued-work batch.
    Batch(BatchId),
}

/// The session's unfinished run ([`RunStore::unfinished_run`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnfinishedRun {
    pub run: TurnId,
    pub head: AdmittedHead,
}

/// What a session run's admission took: the one stored form of
/// `session_runs.admission_json`, written by the session actor's mail drain
/// and read by every host reader (FIG-5221). A turn run takes its inputs or
/// one turn batch, and retains the trace scope its admission proposed; a
/// command or operation run takes the one open batch its commit applies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "run", rename_all = "snake_case")]
pub enum RunAdmissionRecord {
    /// A turn.
    Turn {
        /// The rows the turn executes, bound to its run.
        took: AdmittedTurnRows,
        /// The turn's trace scope, retained by the `turn.admit` commit that
        /// writes this record, before its candidate is selected (FIG-5395).
        /// While the turn is admitted, its admission's export is owed: the
        /// owner that starts the turn reconciles it. `None` when the
        /// admission traced nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace: Option<Box<lash_trace::DurableTraceScope>>,
    },
    /// One session command, applied by its commit.
    Command {
        /// The open command batch.
        batch: BatchId,
    },
    /// A plugin task's operation run, applied by its commit.
    Operation {
        /// The open operation batch.
        batch: BatchId,
    },
}

/// The rows a turn run executes: its inputs, never none, or one turn batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rows", rename_all = "snake_case")]
pub enum AdmittedTurnRows {
    /// Accepted next-turn inputs, in admission order.
    Inputs {
        /// The inputs.
        ids: AdmittedInputIds,
    },
    /// One ready queued-work batch.
    Batch {
        /// The batch.
        id: BatchId,
    },
}

/// A turn's admitted inputs: at least one, in admission order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<InputId>", into = "Vec<InputId>")]
pub struct AdmittedInputIds(Vec<InputId>);

impl AdmittedInputIds {
    /// `ids`, refused when empty: a turn admits at least one input.
    ///
    /// # Errors
    ///
    /// [`EmptyInputAdmission`] when `ids` is empty.
    pub fn new(ids: Vec<InputId>) -> Result<Self, EmptyInputAdmission> {
        if ids.is_empty() {
            Err(EmptyInputAdmission)
        } else {
            Ok(Self(ids))
        }
    }

    /// The first input: the one that heads the turn.
    #[must_use]
    pub fn head(&self) -> &InputId {
        &self.0[0]
    }

    /// Every input, in admission order.
    #[must_use]
    pub fn as_slice(&self) -> &[InputId] {
        &self.0
    }
}

impl TryFrom<Vec<InputId>> for AdmittedInputIds {
    type Error = EmptyInputAdmission;

    fn try_from(ids: Vec<InputId>) -> Result<Self, Self::Error> {
        Self::new(ids)
    }
}

impl From<AdmittedInputIds> for Vec<InputId> {
    fn from(ids: AdmittedInputIds) -> Self {
        ids.0
    }
}

/// A turn admission that names no input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a turn admits at least one input")]
pub struct EmptyInputAdmission;

impl RunAdmissionRecord {
    /// The turn-lane row that heads the run: its first input or its batch.
    #[must_use]
    pub fn head(&self) -> AdmittedHead {
        match self {
            Self::Turn {
                took: AdmittedTurnRows::Inputs { ids },
                ..
            } => AdmittedHead::Input(ids.head().clone()),
            Self::Turn {
                took: AdmittedTurnRows::Batch { id },
                ..
            } => AdmittedHead::Batch(id.clone()),
            Self::Command { batch } | Self::Operation { batch } => {
                AdmittedHead::Batch(batch.clone())
            }
        }
    }

    /// Whether the run is a turn.
    #[must_use]
    pub fn is_turn(&self) -> bool {
        matches!(self, Self::Turn { .. })
    }

    /// The trace scope a turn's admission retained.
    #[must_use]
    pub fn trace(&self) -> Option<&lash_trace::DurableTraceScope> {
        match self {
            Self::Turn { trace, .. } => trace.as_deref(),
            Self::Command { .. } | Self::Operation { .. } => None,
        }
    }

    /// This admission retaining `scope` as its turn's trace scope; a
    /// command or operation run retains none.
    #[must_use]
    pub fn with_trace(self, scope: Option<lash_trace::DurableTraceScope>) -> Self {
        match self {
            Self::Turn { took, .. } => Self::Turn {
                took,
                trace: scope.map(Box::new),
            },
            other => other,
        }
    }

    /// The inputs the run took, in admission order.
    #[must_use]
    pub fn input_ids(&self) -> &[InputId] {
        match self {
            Self::Turn {
                took: AdmittedTurnRows::Inputs { ids },
                ..
            } => ids.as_slice(),
            _ => &[],
        }
    }

    /// The batches the run took: bound to a turn run, open for a command
    /// or operation run.
    #[must_use]
    pub fn batch_ids(&self) -> Vec<BatchId> {
        match self {
            Self::Turn {
                took: AdmittedTurnRows::Batch { id },
                ..
            } => vec![id.clone()],
            Self::Command { batch } | Self::Operation { batch } => vec![batch.clone()],
            Self::Turn {
                took: AdmittedTurnRows::Inputs { .. },
                ..
            } => Vec::new(),
        }
    }

    /// The admission's one stored form: `session_runs.admission_json`.
    ///
    /// # Errors
    ///
    /// [`StoreError::RecordEncodingFailed`] when it does not encode.
    pub fn to_stored(&self) -> Result<String, StoreError> {
        serde_json::to_string(self).map_err(|error| StoreError::RecordEncodingFailed {
            record_kind: "RunAdmissionRecord".to_string(),
            message: error.to_string(),
        })
    }

    /// The admission `stored` holds, decoded by the codec that wrote it.
    ///
    /// # Errors
    ///
    /// [`StoreError::StoredDataCorrupt`] when it does not decode.
    pub fn from_stored(stored: &str) -> Result<Self, StoreError> {
        serde_json::from_str(stored).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "RunAdmissionRecord",
            message: error.to_string(),
        })
    }
}

/// The turns a run executes, and its terminal write ends (FIG-3946): the
/// run's own physical turns, and the turn each member of its admission was
/// accepted under (the member's source key). A member composed into this run
/// never runs as a run of its own, so input addressed to its turn is
/// addressed to this run: accepted while it runs (ADR 0101 §5.1), and
/// next-turn input, or dropped by its cancellation, once it ends.
#[derive(Clone, Debug)]
pub struct RunTurns {
    run: TurnId,
    members: std::collections::BTreeSet<String>,
}

impl RunTurns {
    /// The turns `run` runs, given the turns its admission's rows were
    /// accepted under (their source keys).
    pub fn new(run: &TurnId, members: impl IntoIterator<Item = String>) -> Self {
        Self {
            run: run.clone(),
            members: members.into_iter().collect(),
        }
    }

    /// Whether `turn` is one of the physical turns of a turn this run executes.
    pub fn contains(&self, turn: &TurnId) -> bool {
        let (logical, _) = super::PhysicalTurn::split_turn_id(turn);
        logical == self.run || self.members.contains(logical.as_str())
    }
}

/// A checkpoint's admission request ([`RunStore::admit_at_checkpoint`]).
#[derive(Clone, Debug)]
pub struct CheckpointAdmissionRequest {
    /// The session whose rows the checkpoint admits.
    pub session_id: SessionId,
    /// The logical run whose physical turn reached the checkpoint.
    pub run: TurnId,
    /// The physical turn at the checkpoint: active-turn input addresses it.
    pub turn_id: TurnId,
    pub checkpoint: crate::CheckpointKind,
    /// The checkpoint step's replay key: the admission's `admitted_by`, so a
    /// re-execution of the step reads its own rows back.
    pub step: String,
    pub max_inputs: usize,
    pub policy: crate::TurnLaneAdmissionPolicy,
}

/// What a checkpoint's admission bound to its run: both families, each in
/// `enqueue_seq` order.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointAdmission {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<crate::AdmittedTurnInputs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<crate::AdmittedQueuedWork>,
}

impl CheckpointAdmission {
    /// Whether the checkpoint admitted nothing.
    pub fn is_empty(&self) -> bool {
        self.inputs
            .as_ref()
            .is_none_or(|inputs| inputs.inputs.is_empty())
            && self
                .queued
                .as_ref()
                .is_none_or(|queued| queued.batches.is_empty())
    }
}

/// An in-memory run ledger for store doubles that keep no SQL rows. It
/// decides every write with [`decide_run_terminal_write`], exactly as a SQL
/// backend does inside its transaction.
#[derive(Debug, Default)]
pub struct InMemoryRunLedger {
    state: std::sync::Mutex<InMemoryRuns>,
}

#[derive(Debug, Default)]
struct InMemoryRuns {
    terminals: std::collections::BTreeMap<(SessionId, TurnId), RunTerminal>,
    bindings: std::collections::BTreeMap<(SessionId, InputId), TurnId>,
}

impl InMemoryRunLedger {
    fn state(&self) -> std::sync::MutexGuard<'_, InMemoryRuns> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write `terminal`, deciding it against the stored evidence.
    pub fn write_terminal(&self, terminal: RunTerminal) -> Result<(), StoreError> {
        let mut state = self.state();
        let key = (terminal.session_id.clone(), terminal.run.clone());
        if decide_run_terminal_write(state.terminals.get(&key), &terminal)?
            == RunTerminalWriteDecision::Write
        {
            state.terminals.insert(key, terminal);
        }
        Ok(())
    }

    /// [`RunStore::run_terminal`] over this ledger.
    #[must_use]
    pub fn terminal(&self, session_id: &SessionId, run: &TurnId) -> Option<RunTerminal> {
        self.state()
            .terminals
            .get(&(session_id.clone(), run.clone()))
            .cloned()
    }

    /// [`RunStore::run_binding`] over this ledger.
    #[must_use]
    pub fn binding(&self, session_id: &SessionId, input: &InputId) -> Option<TurnId> {
        self.state()
            .bindings
            .get(&(session_id.clone(), input.clone()))
            .cloned()
    }

    /// [`RunStore::bind_run_inputs`] over this ledger.
    pub fn bind(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        for input in inputs {
            if let Some(bound) = state.bindings.get(&(session_id.clone(), input.clone()))
                && bound != run
            {
                return Err(run_binding_conflict(session_id, input, bound, run));
            }
        }
        for input in inputs {
            state
                .bindings
                .insert((session_id.clone(), input.clone()), run.clone());
        }
        Ok(())
    }

    /// Forget everything session `session_id` holds (its deletion).
    pub fn forget_session(&self, session_id: &SessionId) {
        let mut state = self.state();
        state
            .terminals
            .retain(|(session, _), _| session != session_id);
        state
            .bindings
            .retain(|(session, _), _| session != session_id);
    }
}

/// The refusal of a binding that names another run than the one the input
/// is already bound to.
#[must_use]
pub fn run_binding_conflict(
    session_id: &SessionId,
    input: &InputId,
    bound: &TurnId,
    requested: &TurnId,
) -> StoreError {
    StoreError::Backend(format!(
        "accepted input `{input}` of session `{session_id}` is bound to run `{bound}`; \
         it cannot be bound to run `{requested}`"
    ))
}

impl crate::store::DurableRecord for RunAdmissionRecord {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::compat::SQLITE_CORE_SCHEMA_VERSION);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_id_names_only_its_runs_physical_turns() {
        let run = TurnId::from("host:1");
        assert_eq!(
            TurnCommitId::of_physical_turn(&run, &run),
            Some(TurnCommitId::new(run.clone(), 0))
        );
        let follow_on = crate::store::PhysicalTurn::derive_turn_id(&run, 2);
        assert_eq!(
            TurnCommitId::of_physical_turn(&run, &follow_on),
            Some(TurnCommitId::new(run.clone(), 2))
        );
        assert_eq!(
            TurnCommitId::of_physical_turn(&run, &TurnId::from("other")),
            None
        );
    }
}
