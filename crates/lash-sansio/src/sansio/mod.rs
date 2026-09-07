//! Sans-IO state machine for session turns.
//!
//! `TurnMachine` owns the generic effect engine. Protocol-specific behavior
//! lives behind `ProtocolDriverHandle`, which returns declarative
//! `DriverAction`s that the machine applies.

use std::collections::{HashSet, VecDeque};
use std::fmt::Debug;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::llm::types::{
    AttachmentSource, LlmOutputPart, LlmRequest, LlmResponse, LlmTerminalReason, LlmToolChoice,
    LlmToolSpec, ProviderReplayMeta,
};
use crate::session_model::message::MessageOrigin;
use crate::session_model::{
    Message, MessageRole, MessageSequence, Part, SessionHistoryRecord, SessionStreamEvent,
    TokenUsage, TokenUsageOverflow, TurnTerminationPolicyState, make_error_event,
    reassign_part_ids, render_prompt,
};
use crate::{
    CheckpointKind, ModelToolReturn, PluginMessage, ToolCallOutput, TurnOutcome, TurnStop,
};

// ─── Public types ───

pub trait TurnProtocol: Send + Sync + 'static {
    type Event: Clone + Serialize + DeserializeOwned + Debug + Send + Sync + 'static;
    type Termination: Clone + Default + Debug + Send + Sync + 'static;
    type DriverState: Clone + Default + Serialize + DeserializeOwned + Debug + Send + Sync + 'static;
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct UnitTurnProtocol;

mod turn_protocol;
pub use turn_protocol::{
    ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, CompletedToolCall,
    ContextProjector, DriverAction, DriverContextView, Effect, EffectId, ExecutionEnvironmentSync,
    LlmCallError, LogEvent, PendingToolCall, ProjectorContext, ProtocolDriverHandle, Response,
    TurnCause, TurnMachineConfig, WaitingExecState, WaitingLlmState, render_turn_causes_prompt,
};
mod machine_state;
use machine_state::{EffectDeliveryStatus, MachineState};
pub use machine_state::{TurnCheckpoint, TurnMachine};
mod helpers;
mod turn_machine;
use helpers::{checked_turn_usage_from_llm_usage, refine_terminal_reason_for_context_window};

#[cfg(test)]
mod tests;
