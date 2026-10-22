//! Request-local omission preserves history and attachment occurrence order.
use lash_protocol_types::llm::attachment_delivery::AttachmentPosition;
use lash_protocol_types::{AttachmentMaterializationNotice, AttachmentRef};
use std::sync::Arc;

/// Replace each attachment the target cannot accept with the mode's text.
/// The request is copied on write; the history it was projected from stays intact.
pub fn degrade_unmaterializable_request_attachments(
    request: &mut Arc<crate::llm::types::LlmRequest>,
    accepts: impl Fn(&AttachmentRef, AttachmentPosition) -> bool,
    omission_text: impl Fn(&AttachmentMaterializationNotice) -> String,
) -> Vec<AttachmentMaterializationNotice> {
    use crate::llm::types::LlmContentBlock;
    let mut notices = Vec::new();
    let omitted = |reference: &AttachmentRef, position| !accepts(reference, position);
    if !request
        .messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .any(|block| match block {
            LlmContentBlock::Attachment { reference } => {
                omitted(reference, AttachmentPosition::Message)
            }
            LlmContentBlock::ToolResult { content, .. } => content
                .iter()
                .filter_map(|p| p.attachment())
                .any(|r| omitted(r, AttachmentPosition::ToolResult)),
            _ => false,
        })
    {
        return notices;
    }
    for message in &mut Arc::make_mut(request).messages {
        let mut blocks = message.blocks.to_vec();
        for block in &mut blocks {
            match block {
                LlmContentBlock::Attachment { reference }
                    if omitted(reference, AttachmentPosition::Message) =>
                {
                    let notice = AttachmentMaterializationNotice::target_model_does_not_accept(
                        reference,
                        AttachmentPosition::Message,
                    );
                    *block = LlmContentBlock::Text {
                        text: omission_text(&notice).into(),
                        response_meta: None,
                        cache_breakpoint: false,
                    };
                    notices.push(notice);
                }
                LlmContentBlock::ToolResult { content, .. } => {
                    for part in content {
                        if let Some(reference) = part
                            .attachment()
                            .filter(|r| omitted(r, AttachmentPosition::ToolResult))
                        {
                            let notice =
                                AttachmentMaterializationNotice::target_model_does_not_accept(
                                    reference,
                                    AttachmentPosition::ToolResult,
                                );
                            *part = lash_protocol_types::ModelToolReturnPart::text(omission_text(
                                &notice,
                            ));
                            notices.push(notice);
                        }
                    }
                }
                _ => {}
            }
        }
        message.blocks = Arc::new(blocks.into());
    }
    notices
}
