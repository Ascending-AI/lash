//! Catalogue refusal preserves accepted refs and attachment occurrence order.
use lash_sansio::AttachmentRef;
use lash_sansio::llm::attachment_delivery::AttachmentPosition;
use std::collections::HashSet;
use std::sync::Arc;

pub fn attachment_materialization_notice(
    snapshot: &crate::provider::AttachmentCapabilitySnapshot,
    reference: &AttachmentRef,
    position: AttachmentPosition,
) -> Option<crate::AttachmentMaterializationNotice> {
    snapshot
        .acceptors(&reference.media_type, position)
        .is_empty()
        .then(|| crate::AttachmentMaterializationNotice::no_provider_accepts(reference, position))
}

pub fn degrade_unmaterializable_request_attachments(
    request: &mut Arc<crate::llm::types::LlmRequest>,
) -> Vec<crate::AttachmentMaterializationNotice> {
    use crate::llm::types::LlmContentBlock;
    let mut notices = Vec::new();
    for message in &request.messages {
        for block in message.blocks.iter() {
            match block {
                LlmContentBlock::Attachment { reference } => {
                    if let Some(notice) = attachment_materialization_notice(
                        &request.attachment_acceptance,
                        reference,
                        AttachmentPosition::Message,
                    ) {
                        notices.push(notice);
                    }
                }
                LlmContentBlock::ToolResult { content, .. } => {
                    for reference in content.iter().filter_map(|part| part.attachment()) {
                        if let Some(notice) = attachment_materialization_notice(
                            &request.attachment_acceptance,
                            reference,
                            AttachmentPosition::ToolResult,
                        ) {
                            notices.push(notice);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if notices.is_empty() {
        return notices;
    }
    let request = Arc::make_mut(request);
    let snapshot = &request.attachment_acceptance;
    for message in &mut request.messages {
        let existing = message
            .blocks
            .iter()
            .flat_map(|block| match block {
                LlmContentBlock::Text { text, .. } => vec![text.to_string()],
                LlmContentBlock::ToolResult { content, .. } => content
                    .iter()
                    .filter_map(|part| part.visible_text().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            })
            .collect::<HashSet<_>>();
        Arc::make_mut(&mut message.blocks).retain_mut(|block| match block {
            LlmContentBlock::Attachment { reference } => {
                let Some(notice) = attachment_materialization_notice(
                    snapshot,
                    reference,
                    AttachmentPosition::Message,
                ) else {
                    return true;
                };
                let placeholder = notice.model_placeholder();
                if existing.contains(&placeholder) {
                    return false;
                }
                *block = LlmContentBlock::Text {
                    text: placeholder.into(),
                    response_meta: None,
                    cache_breakpoint: false,
                };
                true
            }
            LlmContentBlock::ToolResult { content, .. } => {
                content.retain_mut(|part| {
                    let Some(notice) = part.attachment().and_then(|reference| {
                        attachment_materialization_notice(
                            snapshot,
                            reference,
                            AttachmentPosition::ToolResult,
                        )
                    }) else {
                        return true;
                    };
                    let placeholder = notice.model_placeholder();
                    if existing.contains(&placeholder) {
                        return false;
                    }
                    *part = lash_sansio::ModelToolReturnPart::text(placeholder);
                    true
                });
                true
            }
            _ => true,
        });
    }
    notices
}
