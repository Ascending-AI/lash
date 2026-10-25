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
    fn decode_cell_call(
        &self,
        decoder: &mut lash_core_store::checkpoint_graph::GraphDecoder<'_>,
        outcome: lash_core_store::checkpoint_graph::NodeId,
    ) -> Result<Option<lash_core::ToolCallRecord>, lash_core_store::checkpoint_graph::GraphRefusal>
    {
        crate::executor::recorded_call(decoder, outcome).map(Some)
    }

    fn decode_cell_ledger(
        &self,
        decoder: &mut lash_core_store::checkpoint_graph::GraphDecoder<'_>,
        root: lash_core_store::checkpoint_graph::NodeId,
    ) -> Result<
        Option<(Vec<lash_core::ToolCallRecord>, Vec<lash_core::ToolCallId>)>,
        lash_core_store::checkpoint_graph::GraphRefusal,
    > {
        crate::executor::recorded_cell_ledger(decoder, root).map(Some)
    }

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
            match super::context::decode_rlm_protocol_event(event)?.as_deref() {
                Some(RlmProtocolEvent::RlmTrajectoryEntry(cell))
                    if !cell.code.trim().is_empty() =>
                {
                    TranscriptItem::Cell(std::sync::Arc::clone(cell))
                }
                Some(
                    RlmProtocolEvent::RlmTrajectoryEntry(_)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Typed stored-data refusal: a corrupt RLM event refuses as the record
    /// it is when a transcript decodes it.
    #[test]
    fn a_corrupt_event_refuses_its_transcript() {
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"format": u32::MAX}),
            serde_json::json!({"format": crate::RLM_PROTOCOL_EVENT_VERSION, "event": null}),
        ] {
            let event = lash_core::ProtocolEvent {
                plugin_id: crate::RLM_PROTOCOL_PLUGIN_ID.into(),
                payload: payload.into(),
            };
            let corruption = RlmTranscriptDecoder
                .decode_event(&event)
                .expect_err("corrupt transcript refuses");
            assert_eq!(corruption.record_kind, "RLM protocol event");
        }
    }
}
