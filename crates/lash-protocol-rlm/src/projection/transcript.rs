use lash_core::transcript::{
    RowContent, RowTool, SuppressionReason, TranscriptDisposition, TranscriptRowKind,
    TranscriptRowProjectorPlugin,
};
use lash_rlm_types::RlmProtocolEvent;

pub(crate) struct RlmTranscriptProjector;
impl TranscriptRowProjectorPlugin for RlmTranscriptProjector {
    fn project_message(&self, message: &lash_core::Message) -> Option<TranscriptDisposition> {
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
            TranscriptDisposition::Suppress(SuppressionReason::SupersededByCommittedReply)
        } else {
            TranscriptDisposition::Render {
                kind: TranscriptRowKind::Reasoning,
                content: RowContent {
                    reasoning,
                    ..Default::default()
                },
            }
        })
    }
    fn project_event(&self, event: &lash_core::ProtocolEvent) -> Option<TranscriptDisposition> {
        if event.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
            return None;
        }
        Some(match super::context::decode_rlm_protocol_event(event) {
            Some(RlmProtocolEvent::RlmAssistantContent(content))
                if !content.reasoning.trim().is_empty() =>
            {
                TranscriptDisposition::Render {
                    kind: TranscriptRowKind::Reasoning,
                    content: RowContent {
                        reasoning: vec![content.reasoning],
                        ..Default::default()
                    },
                }
            }
            Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) if !step.code.trim().is_empty() => {
                let mut output = step
                    .output_archive
                    .as_ref()
                    .map(|archive| archive.witness.clone())
                    .unwrap_or_else(|| {
                        step.output
                            .iter()
                            .map(|print| print.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    });
                if let Some(value) = step.outcome.terminal_value() {
                    let terminal = match value {
                        lash_core::OutputValue::Inline(value) => {
                            serde_json::to_string_pretty(value)
                                .unwrap_or_else(|_| value.to_string())
                        }
                        lash_core::OutputValue::Retained(value) => value.witness.clone(),
                    };
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&terminal);
                }
                let success = !step.outcome.is_failed();
                let error = step.outcome.error().map(|failure| failure.message.clone());
                let tools = step
                    .calls
                    .into_iter()
                    .map(|call| RowTool {
                        operation: call.operation,
                        status: match call.outcome {
                            lash_rlm_types::RlmExecutedCallOutcome::Ok => "success",
                            lash_rlm_types::RlmExecutedCallOutcome::Err => "failure",
                        }
                        .into(),
                    })
                    .collect();
                TranscriptDisposition::Render {
                    kind: TranscriptRowKind::CodeBlock,
                    content: RowContent {
                        language: Some("typescript".into()),
                        code: Some(step.code),
                        output: Some(output),
                        success: Some(success),
                        error,
                        attachments: step.images,
                        tools,
                        tools_omitted: step.calls_omitted,
                        ..Default::default()
                    },
                }
            }
            Some(
                RlmProtocolEvent::RlmAssistantContent(_)
                | RlmProtocolEvent::RlmTrajectoryEntry(_)
                | RlmProtocolEvent::RlmGlobalsPatch(_)
                | RlmProtocolEvent::RlmSeed(_)
                | RlmProtocolEvent::RlmDiagnostic(_),
            ) => TranscriptDisposition::Suppress(SuppressionReason::ProtocolInternal),
            None => TranscriptDisposition::Suppress(SuppressionReason::UnrecognizedProtocolEvent),
        })
    }
}
