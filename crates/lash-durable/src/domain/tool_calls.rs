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

impl ToolCallDecision {
    /// The winner a recorded terminal publishes.
    #[must_use]
    pub fn of(outcome: &lash_core_store::tool_run::AttemptOutcome) -> Self {
        if matches!(
            outcome,
            lash_core_store::tool_run::AttemptOutcome::Cancelled { .. }
        ) {
            Self::Cancel
        } else {
            Self::Final
        }
    }
}

/// Publication of a completed settlement, with the canonical body each
/// settled kind requires. A body-free terminal is settled too. `Body` is the
/// body's root on a row and its decoded record once a read hydrates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolCallAnswer<Body = NodeId> {
    /// The canonical body holds the answer; the row keeps only its winner.
    Materialized {
        decision: ToolCallDecision,
        body: Body,
    },
    /// A recorded execution terminal with no archival body.
    Execution(Box<lash_core_store::tool_run::AttemptOutcome>),
    /// A caller-owned read saved without an attempt.
    Read { body: Body },
}

/// The stored `answer_outcome` column: the answer without its body.
#[derive(serde::Serialize, serde::Deserialize)]
enum StoredOutcome<Execution> {
    Materialized(ToolCallDecision),
    Execution(Execution),
    Read,
}

impl<Body> ToolCallAnswer<Body> {
    #[must_use]
    pub fn body(&self) -> Option<&Body> {
        match self {
            Self::Materialized { body, .. } | Self::Read { body } => Some(body),
            Self::Execution(_) => None,
        }
    }

    #[must_use]
    pub fn into_body(self) -> Option<Body> {
        match self {
            Self::Materialized { body, .. } | Self::Read { body } => Some(body),
            Self::Execution(_) => None,
        }
    }

    #[must_use]
    pub fn cancelled(&self) -> bool {
        match self {
            Self::Materialized { decision, .. } => *decision == ToolCallDecision::Cancel,
            Self::Execution(outcome) => ToolCallDecision::of(outcome) == ToolCallDecision::Cancel,
            Self::Read { .. } => false,
        }
    }

    /// Replace the body, keeping the settled kind.
    ///
    /// # Errors
    ///
    /// Whatever `load` refuses.
    pub fn try_map_body<Loaded, E>(
        self,
        load: impl FnOnce(Body) -> Result<Loaded, E>,
    ) -> Result<ToolCallAnswer<Loaded>, E> {
        Ok(match self {
            Self::Materialized { decision, body } => ToolCallAnswer::Materialized {
                decision,
                body: load(body)?,
            },
            Self::Execution(outcome) => ToolCallAnswer::Execution(outcome),
            Self::Read { body } => ToolCallAnswer::Read { body: load(body)? },
        })
    }
}

impl ToolCallAnswer {
    /// A recorded terminal. With a canonical body it shares that body
    /// instead of copying its material reference; without one it retains
    /// its typed payload.
    #[must_use]
    pub fn terminal(
        outcome: lash_core_store::tool_run::AttemptOutcome,
        body: Option<NodeId>,
    ) -> Self {
        match body {
            Some(body) => Self::Materialized {
                decision: ToolCallDecision::of(&outcome),
                body,
            },
            None => Self::Execution(Box::new(outcome)),
        }
    }

    /// Attach a canonical body to a body-free terminal. False for an
    /// answer that already has its body.
    pub fn archive(&mut self, body: NodeId) -> bool {
        let Self::Execution(outcome) = self else {
            return false;
        };
        *self = Self::Materialized {
            decision: ToolCallDecision::of(outcome),
            body,
        };
        true
    }

    /// The stored `answer_outcome` column; the body is stored beside it.
    ///
    /// # Errors
    ///
    /// A terminal payload that does not encode.
    pub fn stored_outcome(&self) -> serde_json::Result<String> {
        serde_json::to_string(&match self {
            Self::Materialized { decision, .. } => StoredOutcome::Materialized(*decision),
            Self::Execution(outcome) => StoredOutcome::Execution(outcome.as_ref()),
            Self::Read { .. } => StoredOutcome::Read,
        })
    }

    /// Decode the stored columns. `None` is corruption: an outcome that does
    /// not decode, or a body that contradicts the settled kind.
    #[must_use]
    pub fn from_stored(outcome: &str, body: Option<NodeId>) -> Option<Self> {
        let outcome: StoredOutcome<Box<lash_core_store::tool_run::AttemptOutcome>> =
            serde_json::from_str(outcome).ok()?;
        match (outcome, body) {
            (StoredOutcome::Materialized(decision), Some(body)) => {
                Some(Self::Materialized { decision, body })
            }
            (StoredOutcome::Execution(outcome), None) => Some(Self::Execution(outcome)),
            (StoredOutcome::Read, Some(body)) => Some(Self::Read { body }),
            _ => None,
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
#[must_use]
pub fn interrupted_read() -> ToolCallAnswer {
    ToolCallAnswer::terminal(lash_core_store::tool_run::AttemptOutcome::Interrupted, None)
}
