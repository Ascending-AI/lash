//! One sealed logical model call, retained independently of its answer receipt.
//!
//! ModelCallFact stores JSON text. Its graph identity promises the exact
//! stored string, including whitespace and member order. It does not promise
//! a semantic one-spelling identity for the deserialized record.

use crate::checkpoint_graph::{GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind};
use lash_protocol_types::{
    llm::types::AttemptEndReason, llm::types::LlmCallId, llm::types::LlmCallRecord,
    llm::types::LlmUsage, llm::types::TokenUsageOverflow,
};

/// How the logical call ended. An accepted streaming cell is answered even
/// when its remaining tail was interrupted.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallEnd {
    Answered,
    Unanswered { reason: ModelCallEndReason },
}

/// Typed cause for a call that committed no answer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallEndReason {
    Attempt(AttemptEndReason),
    LostWithProcess,
    Unsent,
}

/// Reference-free logical body of a ModelCallFact graph node.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCallFact {
    pub call: u32,
    pub call_id: LlmCallId,
    pub end: ModelCallEnd,
    pub record: LlmCallRecord,
}

impl ModelCallFact {
    /// Stage the sealed fact in its owner's transaction.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        let text = serde_json::to_string(self).map_err(invalid)?;
        Ok(staging.encode_value(NodeKind::MODEL_CALL_FACT, &text)?)
    }

    /// Decode a fact using the read's aggregate graph budget.
    pub fn load(decoder: &mut GraphDecoder<'_>, root: NodeId) -> Result<Self, GraphRefusal> {
        let text: String = decoder.decode(root, NodeKind::MODEL_CALL_FACT)?;
        let fact: Self = serde_json::from_str(&text).map_err(invalid)?;
        if fact.call_id != fact.record.call_id {
            return Err(invalid("the fact and its record name different calls"));
        }
        Ok(fact)
    }
}

fn invalid(error: impl std::fmt::Display) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: NodeKind::MODEL_CALL_FACT.0,
        reason: error.to_string(),
    }
}

/// A settled model call read from its owner's retained rows.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelCallRecord {
    pub call: u32,
    pub call_id: LlmCallId,
    pub end: ModelCallEnd,
    /// Store clock at the transaction that retained the fact.
    pub ended_at_ms: u64,
    pub record: LlmCallRecord,
}

impl ModelCallRecord {
    pub fn usage(&self) -> Result<LlmUsage, TokenUsageOverflow> {
        self.record.usage()
    }
}
