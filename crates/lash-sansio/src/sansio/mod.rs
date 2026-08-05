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
    Message, MessageRole, MessageSequence, Part, PartKind, PruneState, SessionHistoryRecord,
    SessionStreamEvent, TokenUsage, TokenUsageOverflow, TurnTerminationPolicyState,
    make_error_event, reassign_part_ids, render_prompt,
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

include!("sections/turn_protocol.rs");
include!("sections/machine_state.rs");
include!("sections/turn_machine.rs");
include!("sections/helpers.rs");

#[cfg(test)]
mod tests;
