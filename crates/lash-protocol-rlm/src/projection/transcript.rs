use lash_core::transcript::{
    SuppressionReason, TranscriptBlock, TranscriptDecoderPlugin, TranscriptItem, TranscriptMessage,
    TranscriptRole,
};
use lash_rlm_types::RlmProtocolEvent;

/// Decodes the RLM protocol's stored shapes into typed transcript items, the
/// same way for both channels: a cell's assistant context and a refused
/// call's exchange as the committed messages they are, other protocol output
/// as its reasoning, and each trajectory step as the cell record it stores.
pub(crate) struct RlmTranscriptDecoder;
impl TranscriptDecoderPlugin for RlmTranscriptDecoder {
    fn decode_message(&self, message: &lash_core::Message) -> Option<TranscriptItem> {
        if !super::context::is_rlm_protocol_output(message.origin.as_ref()) {
            return None;
        }
        let cell_context = matches!(
            message.origin,
            Some(lash_core::MessageOrigin::TurnOutput {
                cell_id: Some(_),
                ..
            })
        );
        if cell_context
            || crate::native::transport::is_exchange_message(
                message.origin.as_ref(),
                &message.parts,
            )
        {
            let blocks = lash_core::transcript::message_blocks(message);
            let role = if blocks
                .iter()
                .any(|block| matches!(block, TranscriptBlock::ToolResult { .. }))
            {
                TranscriptRole::Tool
            } else {
                TranscriptRole::Assistant
            };
            return Some(if blocks.is_empty() {
                TranscriptItem::Suppressed(SuppressionReason::EmptyContent)
            } else {
                TranscriptItem::Message(TranscriptMessage { role, blocks })
            });
        }
        let reasoning = message
            .parts
            .iter()
            .filter(|part| part.kind() == lash_core::PartKind::Reasoning)
            .map(|part| part.content().into_owned())
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>();
        Some(if reasoning.is_empty() {
            TranscriptItem::Suppressed(SuppressionReason::SupersededByCommittedReply)
        } else {
            reasoning_item(reasoning)
        })
    }
    fn decode_event(
        &self,
        event: &lash_core::ProtocolEvent,
    ) -> Result<Option<TranscriptItem>, lash_core::StoredDataCorruption> {
        if event.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
            return Ok(None);
        }
        Ok(Some(
            match super::context::decode_rlm_protocol_event(event)? {
                Some(RlmProtocolEvent::RlmAssistantContent(content))
                    if !content.reasoning.trim().is_empty() =>
                {
                    reasoning_item(vec![content.reasoning])
                }
                Some(RlmProtocolEvent::RlmTrajectoryEntry(cell))
                    if !cell.code.trim().is_empty() =>
                {
                    TranscriptItem::Cell(cell)
                }
                Some(
                    RlmProtocolEvent::RlmAssistantContent(_)
                    | RlmProtocolEvent::RlmTrajectoryEntry(_)
                    | RlmProtocolEvent::RlmGlobalsPatch(_)
                    | RlmProtocolEvent::RlmSeed(_)
                    | RlmProtocolEvent::RlmDiagnostic(_),
                ) => TranscriptItem::Suppressed(SuppressionReason::ProtocolInternal),
                None => TranscriptItem::Suppressed(SuppressionReason::UnrecognizedProtocolEvent),
            },
        ))
    }
}

fn reasoning_item(reasoning: Vec<String>) -> TranscriptItem {
    TranscriptItem::Message(TranscriptMessage {
        role: TranscriptRole::Assistant,
        blocks: reasoning
            .into_iter()
            .map(|text| TranscriptBlock::Reasoning { text })
            .collect(),
    })
}
