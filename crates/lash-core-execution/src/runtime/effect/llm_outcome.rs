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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmStreamRecord {
    /// The reasoning blocks the live stream already published, so the driver
    /// publishes the completed response's remaining reasoning the same way on
    /// every replay.
    pub reasoning_published: Vec<crate::llm::types::StreamBlockIdentity>,
    /// Each plugin's stream-hook end state, which phase 2's
    /// [`RuntimeEffectCommand::AssistantResponseHooks`](super::RuntimeEffectCommand::AssistantResponseHooks) carries.
    pub stream_hook_states: Vec<AssistantStreamHookState>,
    /// Whether phase 2 follows this completion, decided once with the paid
    /// completion it belongs to.
    pub response_phase: AssistantResponsePhase,
}

impl LlmStreamRecord {
    /// The record of a call whose provider stream published nothing and
    /// whose stream hooks left no state.
    pub fn unstreamed(response_phase: AssistantResponsePhase) -> Self {
        Self {
            reasoning_published: Vec::new(),
            stream_hook_states: Vec::new(),
            response_phase,
        }
    }
}

/// The response phase plan of one LLM call (ADR 0105 §1): whether the served
/// response is the raw completion phase 1 journaled or the one phase 2's
/// assistant-response hooks derive from it.
///
/// Phase 1 records it from the response hooks installed when it ran, and
/// every replay follows the record, never the hook set installed at the
/// replay. A replay of a call recorded [`Self::Raw`] serves the raw
/// completion even where a response hook was installed since; one recorded
/// [`Self::DerivedByHooks`] runs or replays phase 2 even where every
/// response hook was removed since, and a phase 2 that runs with none
/// derives the raw completion unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantResponsePhase {
    /// No response hook was installed: the raw completion is the served
    /// response, and no phase 2 follows.
    Raw,
    /// Response hooks were installed: phase 2 derives the served response.
    DerivedByHooks,
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
