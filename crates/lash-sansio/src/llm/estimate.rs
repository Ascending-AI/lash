//! The request's size before any provider reports it.

use super::types::{LlmContentBlock, LlmRequest, RecordedRequestTemplate, RequestSegment};

impl LlmRequest {
    /// A rough count of the tokens this request's messages and attachments
    /// take, at four bytes a token, never below 1: what a rate limit debits
    /// before the provider reports usage, and the history size a prompt
    /// section reads before any section text is added.
    pub fn estimated_tokens(&self) -> u32 {
        let mut chars = self.model.wire_model().len();
        for message in &self.messages {
            for block in message.blocks.iter() {
                match block {
                    LlmContentBlock::Text { text, .. } => chars += text.len(),
                    LlmContentBlock::ToolCall { input_json, .. } => chars += input_json.len(),
                    LlmContentBlock::ToolResult { content, .. } => {
                        for part in content {
                            chars += match part {
                                crate::ModelToolReturnPart::Text { text } => text.len(),
                                crate::ModelToolReturnPart::Retained(retained) => {
                                    retained.witness.len()
                                }
                                crate::ModelToolReturnPart::Attachment(_) => 256,
                            };
                        }
                    }
                    LlmContentBlock::Reasoning { text, .. } => chars += text.len(),
                    LlmContentBlock::Attachment { .. } => chars += 256,
                }
            }
        }
        chars = chars.saturating_add(
            self.attachments()
                .map(|reference| usize::try_from(reference.byte_len / 4).unwrap_or(usize::MAX))
                .fold(0usize, usize::saturating_add),
        );
        ((chars / 4).max(1)).try_into().unwrap_or(u32::MAX)
    }
}

impl RecordedRequestTemplate {
    /// A rough count of the tokens this body takes, at four bytes a token,
    /// never below 1: its literals, and each slot as a request's estimate
    /// counts its attachment. It is what a rate limit debits for a send,
    /// which has the body and no request.
    pub fn estimated_tokens(&self) -> u32 {
        let chars = self
            .segments()
            .iter()
            .map(|segment| match segment {
                RequestSegment::Literal { text } => text.len(),
                RequestSegment::Attachment { slot } => 256usize.saturating_add(
                    usize::try_from(slot.reference.byte_len / 4).unwrap_or(usize::MAX),
                ),
            })
            .fold(0usize, usize::saturating_add);
        ((chars / 4).max(1)).try_into().unwrap_or(u32::MAX)
    }
}
