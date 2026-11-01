//! Retained call identities and publications, independent of replay journals.

use lash_core_store::checkpoint_graph::NodeId;
use lash_protocol_types::ToolCallId;

use super::{OwnerKey, RunFacts, RunSeq};

/// The retained lifetime and lookup namespace, independent of execution cells.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CallOwner {
    Turn(
        lash_kernel_value::identity::SessionId,
        lash_vocabulary::identity::TurnId,
    ),
    Process(lash_vocabulary::identity::ProcessId),
    Operation(lash_kernel_value::identity::SessionId, String),
}
impl CallOwner {
    pub fn stored(&self) -> String {
        match self {
            Self::Turn(session, run) => super::keys::stored("t", &[session.as_str(), run.as_str()]),
            Self::Process(process) => super::keys::stored("p", &[process.as_str()]),
            Self::Operation(session, operation) => {
                super::keys::stored("o", &[session.as_str(), operation])
            }
        }
    }
    pub fn from_opener(opener: &lash_core_store::effect_opener::EffectOpener) -> Self {
        use lash_core_store::effect_opener::EffectOpener;
        match opener {
            EffectOpener::Turn {
                session_id,
                turn_id,
            } => Self::Turn(session_id.clone(), turn_id.clone()),
            EffectOpener::Process { process_id } => Self::Process(process_id.clone()),
            EffectOpener::SessionOperation {
                session_id,
                operation_id,
            } => Self::Operation(session_id.clone(), operation_id.clone()),
        }
    }
}

/// A cell defaults to its enclosing turn; tool-bearing operations pass their opener.
#[must_use]
pub fn logical_owner(owner: &OwnerKey) -> CallOwner {
    match owner {
        OwnerKey::Turn(session, turn) | OwnerKey::Cell(session, turn, _) => {
            CallOwner::Turn(session.clone(), turn.clone())
        }
        OwnerKey::Process(process) => CallOwner::Process(process.clone()),
    }
}

/// The request already records the logical opener used by operation-hosted cells.
#[must_use]
pub fn request_owner(
    execution: &OwnerKey,
    request: &lash_core_store::tool_run::MaterialRef,
) -> CallOwner {
    match &request.owner {
        lash_core_store::tool_run::MaterialOwner::Run { opener } => CallOwner::from_opener(opener),
        lash_core_store::tool_run::MaterialOwner::Process { process_id } => {
            CallOwner::Process(process_id.clone())
        }
        _ => logical_owner(execution),
    }
}

/// The committed winner, before its protected work necessarily finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolCallDecision {
    Final,
    Cancel,
}

/// Publication of a completed settlement. A body-free terminal is settled too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallAnswer {
    pub outcome: ToolCallSettlement,
    pub body: Option<NodeId>,
}

/// A recorded execution terminal or a caller-owned read saved without an attempt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolCallSettlement {
    /// The canonical body holds the answer; only its winner is needed here.
    Materialized(ToolCallDecision),
    Execution(Box<lash_core_store::tool_run::AttemptOutcome>),
    Read,
}

impl ToolCallSettlement {
    /// Keep the terminal payload out of each directory row and batch slot.
    pub fn execution(outcome: lash_core_store::tool_run::AttemptOutcome) -> Self {
        Self::Execution(Box::new(outcome))
    }
}

impl ToolCallAnswer {
    /// Body-free terminals retain their typed payload. Materialized answers
    /// share the canonical body instead of copying its material reference.
    pub fn execution(
        outcome: lash_core_store::tool_run::AttemptOutcome,
        body: Option<NodeId>,
    ) -> Self {
        let outcome = if body.is_some() {
            ToolCallSettlement::Materialized(
                if matches!(
                    outcome,
                    lash_core_store::tool_run::AttemptOutcome::Cancelled { .. }
                ) {
                    ToolCallDecision::Cancel
                } else {
                    ToolCallDecision::Final
                },
            )
        } else {
            ToolCallSettlement::execution(outcome)
        };
        Self { outcome, body }
    }

    pub fn valid(&self) -> bool {
        matches!(
            (&self.outcome, self.body),
            (ToolCallSettlement::Execution(_), None)
                | (
                    ToolCallSettlement::Materialized(_) | ToolCallSettlement::Read,
                    Some(_)
                )
        )
    }

    pub fn cancelled(&self) -> bool {
        match &self.outcome {
            ToolCallSettlement::Materialized(decision) => *decision == ToolCallDecision::Cancel,
            ToolCallSettlement::Execution(outcome) => matches!(
                outcome.as_ref(),
                lash_core_store::tool_run::AttemptOutcome::Cancelled { .. }
            ),
            ToolCallSettlement::Read => false,
        }
    }
}

/// Changes to one call, always committed with its existing owner transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolCallWrite {
    /// Explicit logical-owner retention expiry, never replay compaction.
    Expire { owner: CallOwner },
    Admit {
        /// An execution admission consumes the turn budget; a caller save does not.
        counted: bool,
        logical: CallOwner,
        execution: OwnerKey,
        run: RunSeq,
        member: u64,
        call: ToolCallId,
        cancellable: bool,
    },
    Decide {
        owner: OwnerKey,
        call: ToolCallId,
        decision: ToolCallDecision,
        fact: NodeId,
    },
    Answer {
        owner: OwnerKey,
        call: ToolCallId,
        answer: ToolCallAnswer,
    },
    /// A declaration-free final publishes its decision and answer atomically.
    Complete {
        owner: OwnerKey,
        call: ToolCallId,
        fact: NodeId,
        answer: ToolCallAnswer,
    },
}

/// Metadata selected before graph hydration, with no replay dependencies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallRow {
    pub sequence: u64,
    pub owner: CallOwner,
    pub call: ToolCallId,
    pub execution: OwnerKey,
    pub run: RunSeq,
    pub member: u64,
    pub decision: Option<ToolCallDecision>,
    pub decision_fact: Option<NodeId>,
    pub answer: Option<ToolCallAnswer>,
}

/// The two independently published immutable graph roots of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolCallRoot {
    Decision,
    Answer,
}

/// Only the selected rows' archival bodies, loaded in one protected read.
#[derive(Clone, Debug, Default)]
pub struct ToolCalls {
    pub rows: Vec<ToolCallRow>,
    pub graph: RunFacts,
}

/// A stable cursor in the owner's indexed admission order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallCursor {
    pub sequence: u64,
    pub call: ToolCallId,
}

/// Explicit selection: keyed reads never enumerate an owner's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolCallSelection {
    Calls(Vec<ToolCallId>),
    List {
        after: Option<ToolCallCursor>,
        limit: u32,
    },
    SavedReads {
        after: Option<ToolCallCursor>,
        limit: u32,
    },
}

/// A saved plan whose owner ended before saving any answer never remains Pending.
pub fn interrupted_read() -> ToolCallSettlement {
    ToolCallSettlement::execution(lash_core_store::tool_run::AttemptOutcome::Interrupted)
}
