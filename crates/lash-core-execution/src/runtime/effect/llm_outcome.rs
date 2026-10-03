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
    /// Each response callback's stream end state, which phase 2's
    /// [`RuntimeEffectCommand::AssistantResponseHooks`](super::RuntimeEffectCommand::AssistantResponseHooks) carries.
    pub stream_hook_states: Vec<AssistantStreamHookState>,
    /// The exact ordered callbacks selected before the paid completion.
    pub response_plan: AssistantResponsePlan,
}

impl LlmStreamRecord {
    /// The record of a call whose provider stream published nothing and
    /// whose stream hooks left no state.
    pub fn unstreamed(response_plan: AssistantResponsePlan) -> Self {
        Self {
            reasoning_published: Vec::new(),
            stream_hook_states: Vec::new(),
            response_plan,
        }
    }
}

/// The ordered response callbacks selected before one paid LLM call.
///
/// Phase 1 journals this plan with its raw completion. An unfinished
/// derivation resolves these exact keys and owning revisions before invoking
/// any callback, then executes in recorded order. An empty plan serves the
/// raw completion. Completed derivations replay without resolving callbacks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantResponsePlan {
    pub callbacks: Vec<crate::plugin::PluginCallbackIdentity>,
}

/// The stream end state for one recorded response callback.
///
/// A response registration names its stream-finished callback by key within
/// the same plugin. Phase 1 records the receiving callback's full
/// identity, so multiple callbacks and different revisions cannot share state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantStreamHookState {
    pub callback: crate::plugin::PluginCallbackIdentity,
    pub state: serde_json::Value,
}
