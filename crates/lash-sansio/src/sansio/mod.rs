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

use crate::llm::types::{
    LlmOutputPart, LlmRequest, LlmResponse, LlmTerminalReason, LlmToolChoice, LlmToolSpec,
    ProviderReplayMeta,
};
use crate::session_model::{
    LlmUsage, Message, MessageSequence, SessionHistoryRecord, SessionStreamEvent,
    TokenUsageOverflow, make_error_event,
};
use crate::{CheckpointKind, ModelToolReturn, ToolCallOutput, TurnOutcome, TurnStop};

// ─── Public types ───

pub trait TurnProtocol: Send + Sync + 'static {
    type IntentOutcome: Clone + Serialize + DeserializeOwned + Debug + Send + Sync + 'static;
    type Event: Clone + Serialize + DeserializeOwned + Debug + Send + Sync + 'static;
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
    ExpandedWrapper, LlmCallError, LogEvent, ModelToolCalls, PendingToolCall, PendingWork,
    ProjectorContext, ProtocolDriverHandle, Response, ResponseToolCalls, SyncedEnvironment,
    ToolExpansionPlan, TurnMachineConfig, place_prompt, stored_history_refusal_actions,
};
mod checkpoint_content;
pub use checkpoint_content::{CheckpointContentRef, TurnCheckpointContent};
mod turn_window;
pub use turn_window::{TurnWindow, TurnWindowPin};
mod machine_state;
use machine_state::{
    CheckpointMessages, CheckpointState, CheckpointWindow, EffectDeliveryStatus, MachineState,
    ProgressBoundary, RunAbort,
};
pub use machine_state::{
    SavedTurn, TURN_CHECKPOINT_SCHEMA_VERSION, TurnCheckpoint, TurnCheckpointRestoreError,
    TurnMachine,
};
mod helpers;
mod turn_machine;
use helpers::{checked_turn_usage_from_llm_usage, refine_terminal_reason_for_context_window};

#[cfg(test)]
mod tests;
