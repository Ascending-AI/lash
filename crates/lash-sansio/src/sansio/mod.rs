//! Sans-IO state machine for session turns.
//!
//! `TurnMachine` owns the generic effect engine. Protocol-specific behavior
//! lives behind `ProtocolDriverHandle`, which returns declarative
//! `DriverAction`s that the machine applies.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use lash_protocol_types::LlmCallError;
use lash_protocol_types::{CheckpointKind, ModelToolReturn, ToolCallOutput, TurnOutcome, TurnStop};
use lash_protocol_types::{
    llm::types::LlmOutputPart, llm::types::LlmRequest, llm::types::LlmResponse,
    llm::types::LlmTerminalReason, llm::types::LlmToolChoice, llm::types::LlmToolSpec,
    llm::types::ProviderReplayMeta,
};
use lash_protocol_types::{
    session_model::LlmUsage, session_model::Message, session_model::MessageSequence,
    session_model::SessionHistoryRecord, session_model::SessionStreamEvent,
    session_model::TokenUsageOverflow, session_model::make_error_event,
};

// ─── Public types ───

pub trait TurnProtocol: Send + Sync + 'static {
    type IntentOutcome: Clone + Serialize + DeserializeOwned + Debug + Send + Sync + 'static;
    type Event: Clone + PartialEq + Serialize + DeserializeOwned + Debug + Send + Sync + 'static;
    type Termination: Clone + Default + Debug + Send + Sync + 'static;
    type DriverState: Clone + Default + Serialize + DeserializeOwned + Debug + Send + Sync + 'static;
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct UnitTurnProtocol;

mod turn_protocol;
use turn_protocol::AnsweredWork;
pub use turn_protocol::{
    ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, CompletedToolCall,
    ContextProjector, DriverAction, DriverContextView, Effect, EffectId, ExecutionEnvironmentSync,
    ExecutionEnvironmentSyncFailure, ExecutionEnvironmentSyncFailureKind, ExpandedRow,
    ExpandedWrapper, LogEvent, ModelToolCalls, PendingToolCall, PendingWork, ProjectorContext,
    ProtocolDriverHandle, Response, ResponseToolCalls, SyncedEnvironment, ToolExpansionPlan,
    ToolWave, TurnMachineConfig, UndecodableDriverState, place_prompt,
    stored_history_refusal_actions,
};
mod checkpoint_content;
pub use checkpoint_content::{CheckpointContentPin, CheckpointContentRef, TurnCheckpointContent};
mod checkpoint_ledger;
pub use checkpoint_ledger::CheckpointDelta;
use checkpoint_ledger::{
    CheckpointMessages, CheckpointPrompt, ContentLedger, Floors, Sequence, StoredChains,
    StoredRequest,
};
mod turn_window;
pub use turn_window::{TurnWindow, TurnWindowPin};
mod machine_state;
use machine_state::{
    CheckpointState, CheckpointWindow, EffectDeliveryStatus, MachineState, ProgressBoundary,
    RunAbort,
};
pub use machine_state::{SavedTurn, TurnCheckpoint, TurnCheckpointRestoreError, TurnMachine};
mod helpers;
mod turn_machine;
use helpers::{checked_turn_usage_from_llm_usage, refine_terminal_reason_for_context_window};

#[cfg(test)]
mod tests;

use lash_protocol_types::TURN_CHECKPOINT_SCHEMA_VERSION;
