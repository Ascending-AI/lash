//! The immutable commit request, and commit, park and settlement as recorded
//! steps (ADR 0105 §9).
//!
//! The request holds no runtime state: no `Arc`, `Mutex`, clock, session
//! state or graph editor. It is built by a pure function of the machine state
//! and the recorded outcomes, and its body checks the fence, the commit id and
//! the root's terminality in the same store transaction as the head
//! compare-and-set.
//!
//! The component types below carry what the plan names for each. The slice
//! that first issues `CommitTurn` (P15) owns their final shape and may change
//! any of them before a byte of them is persisted; none is a registered
//! durable format yet.

use serde::{Deserialize, Serialize};

use super::context::EpochMs;
pub use crate::store::SessionHeadRef;
use crate::store::{BlobRef, ParkReason, StoreError};
use crate::{
    AttachmentId, BatchId, FrameNodeId, InputId, NodeId, PluginState, ProtocolTurnOptions,
    SessionId, SessionPolicy, TokenUsage, TurnCancellationEvidence, TurnId, TurnStop,
};

/// One turn's commit, serialized and immutable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCommitRequest {
    /// The registered durable surface this request is encoded at.
    pub format: u32,
    pub session: SessionId,
    /// The logical root.
    pub root: TurnId,
    /// Derived from the root and the physical ordinal; never from an epoch or
    /// a clock.
    pub commit_id: TurnCommitId,
    /// Generation and revision for the head compare-and-set.
    pub expected_head: SessionHeadRef,
    /// Nodes with their final ids; timestamps from recorded time.
    pub graph_delta: SessionGraphDelta,
    pub state: DurableTurnState,
    /// The outcome of the `CaptureExecutionState` step.
    pub execution_state: Option<ExecutionStateUpdate>,
    /// The outcome of the `CapturePluginStates` step.
    pub plugin_states: RecordedPluginStates,
    pub usage: Vec<UsageDelta>,
    pub ingress: IngressSettlement,
    pub cancellation: Option<CancellationSettlement>,
    pub attachments: CommittedAttachments,
    /// Root-addressed terminal evidence.
    pub terminal: TurnTerminalEvidence,
}

/// What a `CommitTurn` step recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommitTurnOutcome {
    Committed {
        head: SessionHeadRef,
    },
    /// A retried body found its own commit already there (a lost reply): the
    /// same commit id and the same bytes.
    AlreadyCommitted {
        head: SessionHeadRef,
    },
    /// Another commit already closed this root. It is preserved and never
    /// replaced; a later epoch adopts it (ADR 0105 law L-S6).
    RootAlreadyTerminal {
        by: TurnCommitId,
    },
    StaleFence {
        current_epoch: u64,
    },
    HeadConflict {
        found: SessionHeadRef,
    },
}

/// A turn commit's identity: the logical root and the physical ordinal of
/// the attempt that commits it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnCommitId {
    root: TurnId,
    ordinal: u32,
}

impl TurnCommitId {
    pub fn new(root: TurnId, ordinal: u32) -> Self {
        Self { root, ordinal }
    }

    pub fn root(&self) -> &TurnId {
        &self.root
    }

    pub fn ordinal(&self) -> u32 {
        self.ordinal
    }
}

/// The graph nodes one commit appends, with final ids.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGraphDelta {
    pub nodes: Vec<CommittedGraphNode>,
}

/// One appended node, its body in the durable graph-node encoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedGraphNode {
    pub node_id: NodeId,
    pub parent_node_id: Option<NodeId>,
    pub recorded_at: EpochMs,
    pub body: serde_json::Value,
}

/// The durable turn state a commit writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableTurnState {
    pub policy: SessionPolicy,
    pub turn_index: u64,
    pub token_usage: TokenUsage,
    pub last_prompt_usage: Option<TokenUsage>,
    pub current_frame_node_id: Option<FrameNodeId>,
    pub protocol_turn_options: ProtocolTurnOptions,
}

/// The protocol-owned execution-state change a commit applies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
pub enum ExecutionStateUpdate {
    /// Replace the state with the captured body at this ref.
    Replace {
        state_ref: BlobRef,
    },
    Clear,
}

/// The plugin states the `CapturePluginStates` step recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedPluginStates {
    pub plugins: PluginState,
}

/// One cost-ledger contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageDelta {
    pub source: String,
    pub model: String,
    pub usage: TokenUsage,
}

/// What the commit settles on the session ingress.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressSettlement {
    /// Claims the turn completed.
    pub completed: Vec<InputId>,
    /// Work the turn withheld, released for a later turn.
    pub withheld: Vec<InputId>,
    /// Batches the turn enqueued.
    pub enqueued: Vec<BatchId>,
}

/// A cancelled turn's settlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellationSettlement {
    /// The tool calls the cancel interrupted.
    pub interrupted: Vec<String>,
    /// The cancel request's evidence, which names its intent.
    pub evidence: TurnCancellationEvidence,
    /// Whether the cancel authorizes closing the turn's lifetime scopes.
    pub closure_authorized: bool,
}

/// The attachments a commit makes durable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedAttachments {
    pub committed: Vec<AttachmentId>,
    /// Upload intents the commit adopts.
    pub adopted_intents: Vec<String>,
}

/// Terminal evidence, addressed by the logical root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTerminalEvidence {
    pub root: TurnId,
    /// The physical turn that reached the terminal.
    pub turn: TurnId,
    /// Why the turn stopped; `None` when it completed.
    pub stop: Option<TurnStop>,
}

/// A park record's id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParkId(String);

impl ParkId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The work a park names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedWorkRef {
    pub session: SessionId,
    pub root: TurnId,
}

/// The engine's own reference to a stalled execution. Opaque to lash.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EngineParkRef(String);

impl EngineParkRef {
    pub fn new(reference: impl Into<String>) -> Self {
        Self(reference.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The recovery writer for a park the workflow cannot record itself, for
/// example when a nondeterministic workflow task stops it before `RecordPark`.
/// Its idempotency key is the root and the park id, the same as `RecordPark`'s,
/// so the two writers converge. Park reconciliation is its only caller.
#[async_trait::async_trait]
pub trait ParkRecoveryWriter: Send + Sync {
    async fn record_engine_park(
        &self,
        root: &ParkedWorkRef,
        reason: ParkReason,
        engine: EngineParkRef,
    ) -> Result<ParkId, StoreError>;
}
