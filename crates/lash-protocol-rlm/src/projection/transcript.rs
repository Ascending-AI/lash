use lash_core::transcript::{
    CellPrint, CellResult, SuppressionReason, TranscriptBlock, TranscriptCell,
    TranscriptDecoderPlugin, TranscriptItem, TranscriptMessage, TranscriptRole,
};
use lash_rlm_types::RlmProtocolEvent;

/// Decodes the RLM protocol's stored shapes into typed transcript items:
/// its reasoning into assistant reasoning blocks and each trajectory step
/// into a code cell with its typed outcome.
pub(crate) struct RlmTranscriptDecoder;
impl TranscriptDecoderPlugin for RlmTranscriptDecoder {
    fn decode_message(&self, message: &lash_core::Message) -> Option<TranscriptItem> {
        if !super::context::is_rlm_protocol_output(message.origin.as_ref()) {
            return None;
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
                Some(RlmProtocolEvent::RlmTrajectoryEntry(step))
                    if !step.code.trim().is_empty() =>
                {
                    TranscriptItem::Cell(Box::new(TranscriptCell {
                        language: "typescript".into(),
                        code: step.code,
                        prints: step
                            .output
                            .into_iter()
                            .map(|print| CellPrint {
                                text: print.text,
                                value: print.value,
                            })
                            .collect(),
                        prints_retained: step.output_archive.map(|archive| *archive),
                        result: match step.outcome {
                            lash_rlm_types::CellOutcome::Running => CellResult::Completed,
                            lash_rlm_types::CellOutcome::Failed(failure) => {
                                CellResult::Failed(failure)
                            }
                            lash_rlm_types::CellOutcome::Finished(value) => {
                                CellResult::Finished(value.into())
                            }
                        },
                        calls: step.calls,
                        calls_omitted: step.calls_omitted,
                        images: step.images,
                    }))
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
