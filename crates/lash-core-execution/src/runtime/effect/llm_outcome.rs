//! What a turn's LLM step recorded, decoded for the driver.

use serde::{Deserialize, Serialize};

/// What phase 1 of a turn's LLM call recorded, decoded for the driver.
#[derive(Debug)]
pub struct RuntimeLlmCallOutcome {
    pub result: Result<crate::llm::types::LlmResponse, crate::sansio::LlmCallError>,
    pub text_streamed: bool,
    pub call_record: Option<crate::LlmCallRecord>,
    pub stream: LlmStreamRecord,
}

/// What a turn's provider stream left behind that later steps read: recorded
/// with phase 1's outcome so a replay reads it from the journal, never from
/// the memory of the worker that streamed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmStreamRecord {
    /// The reasoning blocks the live stream already published, so the driver
    /// publishes the completed response's remaining reasoning the same way on
    /// every replay.
    pub reasoning_published: Vec<crate::llm::types::StreamBlockIdentity>,
    /// Each plugin's stream-hook end state, which phase 2's
    /// [`RuntimeEffectCommand::AssistantResponseHooks`](super::RuntimeEffectCommand::AssistantResponseHooks) carries.
    pub stream_hook_states: Vec<AssistantStreamHookState>,
}

/// The state one plugin's stream hooks reached when the provider stream
/// finished (see [`crate::plugin::AssistantStreamFinishedHook`]).
///
/// Recorded with phase 1's outcome and handed to the same plugin's
/// assistant-response hook in phase 2, so the derivation never depends on
/// which worker streamed the completion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantStreamHookState {
    pub plugin_id: String,
    pub state: serde_json::Value,
}
