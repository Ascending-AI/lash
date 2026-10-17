//! What a turn's LLM step recorded, decoded for the driver.

use serde::{Deserialize, Serialize};

/// What one attempt of a turn's LLM call returned, decoded for the driver:
/// the call's receipt-to-be.
#[derive(Debug)]
pub struct RuntimeLlmCallOutcome {
    pub result: Result<crate::llm::types::LlmResponse, crate::sansio::LlmCallError>,
    pub text_streamed: bool,
    pub call_record: Option<crate::LlmCallRecord>,
    pub stream: LlmStreamRecord,
}

/// What a turn's provider stream left behind that the call's derivation
/// reads: recorded in the call's receipt, so a derivation reads it from the
/// receipt, never from the memory of the worker that streamed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmStreamRecord {
    /// Actual provider attempt whose stream the receipt derives, even when
    /// its accepted tail has not sealed a call record yet.
    pub attempt: Option<u32>,
    /// The reasoning blocks the live stream already published, so the driver
    /// publishes the completed response's remaining reasoning the same way on
    /// every derivation.
    pub reasoning_published: Vec<crate::llm::types::StreamBlockIdentity>,
    /// What the call's stream hooks saw, from which a derivation finishes
    /// them on any worker.
    pub capsule: AssistantStreamCapsule,
    /// The exact ordered callbacks selected before the paid completion.
    pub response_plan: AssistantResponsePlan,
}

impl LlmStreamRecord {
    /// The callbacks a derivation of the call's outcome invokes, in
    /// recorded order: the response plan's over a successful outcome, then
    /// those that finish its stream ([`AssistantStreamCapsule::finishing`]).
    /// A receipt owes exactly these until it is derived (S7), so a node that
    /// cannot resolve one never claims the turn to derive it, and a node
    /// that can derives it.
    pub fn derivation_callbacks(
        &self,
        succeeded: bool,
    ) -> Vec<crate::plugin::PluginCallbackIdentity> {
        let plan = if succeeded {
            &self.response_plan.callbacks[..]
        } else {
            &[]
        };
        let (stream, finished) = self.capsule.finishing();
        plan.iter().chain(finished).chain(stream).cloned().collect()
    }
}

/// A paid call's stream as its stream hooks saw it: the stream callbacks
/// and stream-finished callbacks the call ran under, every visible chunk
/// the stream callbacks were handed since the attempt that answered began,
/// in order, and how the stream finished.
///
/// The live stream's hooks are never finished before the call's receipt is
/// acknowledged. Every derivation of the call's outcome, successful or not,
/// and the settlement of one a cancel stopped, discards what the finished
/// callbacks hold, replays the chunks through the recorded stream callbacks
/// and finishes them with the recorded reason
/// ([`PluginSession::replay_assistant_stream`](crate::plugin::PluginSession::replay_assistant_stream)),
/// so every terminal reason reaches them, the stream-state handed to the
/// response callbacks is the same on the worker that streamed and on any
/// other, and nothing depends on the streaming worker's memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantStreamCapsule {
    /// The stream callbacks, in recorded order.
    pub stream_callbacks: Vec<crate::plugin::PluginCallbackIdentity>,
    /// The stream-finished callbacks, in recorded order.
    pub finished_callbacks: Vec<crate::plugin::PluginCallbackIdentity>,
    /// The visible chunks the stream callbacks were handed, raw.
    pub chunks: Vec<String>,
    /// How the stream finished.
    pub finish: crate::plugin::AssistantStreamFinishReason,
}

impl AssistantStreamCapsule {
    /// The capsule of a call that streams under `stream_callbacks` and
    /// `finished_callbacks`, before its first chunk.
    pub fn new(
        stream_callbacks: Vec<crate::plugin::PluginCallbackIdentity>,
        finished_callbacks: Vec<crate::plugin::PluginCallbackIdentity>,
    ) -> Self {
        Self {
            stream_callbacks,
            finished_callbacks,
            chunks: Vec::new(),
            finish: crate::plugin::AssistantStreamFinishReason::Complete,
        }
    }

    /// The stream callbacks and the stream-finished callbacks that finishing
    /// the stream from the capsule invokes, each in recorded order: none
    /// when no stream-finished callback watched it, and the stream callbacks
    /// only when the stream handed them chunks, which they are handed
    /// again.
    pub fn finishing(
        &self,
    ) -> (
        &[crate::plugin::PluginCallbackIdentity],
        &[crate::plugin::PluginCallbackIdentity],
    ) {
        if self.finished_callbacks.is_empty() {
            return (&[], &[]);
        }
        let stream = if self.chunks.is_empty() {
            &[][..]
        } else {
            &self.stream_callbacks[..]
        };
        (stream, &self.finished_callbacks)
    }

    /// Whether the stream produced a response the stream-finished callbacks
    /// hand state for.
    pub fn produced_response(&self) -> bool {
        matches!(
            self.finish,
            crate::plugin::AssistantStreamFinishReason::Complete
                | crate::plugin::AssistantStreamFinishReason::Aborted
        )
    }
}

/// The ordered response callbacks selected before one paid LLM call.
///
/// The call's receipt records this plan with its raw completion. A
/// derivation resolves these exact keys and owning plugin ids before invoking
/// any callback, then executes in recorded order. An empty plan serves the
/// raw completion.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantResponsePlan {
    pub callbacks: Vec<crate::plugin::PluginCallbackIdentity>,
}

/// The stream end state for one recorded response callback.
///
/// A response registration names its stream-finished callback by key within
/// the same plugin. The derivation hands the state to the receiving
/// callback's full identity in its plugin's format, so callbacks cannot share
/// state or read an undeclared format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantStreamHookState {
    pub callback: crate::plugin::PluginCallbackIdentity,
    pub format: crate::plugin::FormatVersion,
    pub state: serde_json::Value,
}
