//! The turn's prepared context. Its messages are the turn's history as the
//! request sees it, after the attachment-omission history policies (ADR
//! 0133). Durable compaction is an explicit Agent Frame transition, not a
//! rewrite of this prepared context. Tool availability is catalog
//! membership, never a prepared-context choice.

/// The messages the runtime hands to the LLM call.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PreparedContext {
    pub messages: crate::MessageSequence,
}
