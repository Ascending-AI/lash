use crate::{
    TraceContentBlock, TraceEvent, TraceLlmMessage, TraceToolCallOutcome, TraceToolResultBlock,
    TraceToolSpec,
};
use lash_sansio::llm::types::LlmOutputPart;
use serde_json::{Value, json};
fn attachment(reference: &crate::TraceAttachment) -> Value {
    json!({"type":"lash.attachment","mime_type":reference.media_type,"lash.attachment.id":reference.id,
        "byte_len":reference.byte_len,"delivery_form":reference.delivery_form})
}
pub(super) fn semconv_input(messages: &[TraceLlmMessage]) -> Value {
    Value::Array(messages.iter().map(|message|json!({"role":message.role,"parts":message.blocks.iter().filter_map(|block|match block {
        TraceContentBlock::Text{text,..}=>Some(json!({"type":"text","content":text})),
        TraceContentBlock::Attachment{reference}=>Some(attachment(reference)),
        TraceContentBlock::ToolCall{call_id,tool_name,input_json,..}=>Some(json!({"type":"tool_call","id":call_id,"name":tool_name,"arguments":input_json})),
        TraceContentBlock::ToolResult{call_id,content,..}=>Some(json!({"type":"tool_call_response","id":call_id,"response":content.iter().map(|part|match part {
            TraceToolResultBlock::Text{text}=>json!({"type":"text","content":text}),TraceToolResultBlock::Attachment{reference}=>attachment(reference)
        }).collect::<Vec<_>>()})),
        TraceContentBlock::Reasoning{text,has_encrypted,redacted,..}=>if *has_encrypted || *redacted {None} else {Some(json!({"type":"reasoning","content":text}))},
    }).collect::<Vec<_>>() })).collect())
}
pub(super) fn semconv_tools(tools: &[TraceToolSpec]) -> Value {
    Value::Array(tools.iter().map(|tool|json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema})).collect())
}
fn object(value: &Value) -> Value {
    if value.is_object() {
        value.clone()
    } else {
        json!({"value":value})
    }
}
/// The complete structured content attributes; no consumer serializes the event as a blob.
pub(crate) fn attributes(event: &TraceEvent) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    let request = match event {
        TraceEvent::LlmCallCompleted { request, .. }
        | TraceEvent::LlmCallFailed { request, .. } => Some(request),
        _ => None,
    };
    if let Some(request) = request {
        if let Some(text) = &request.system_instructions {
            out.push((
                "gen_ai.system_instructions",
                json!([{"type":"text","content":text}]),
            ));
        }
        if !request.messages.is_empty() {
            out.push(("gen_ai.input.messages", semconv_input(&request.messages)));
        }
        if request
            .tools
            .iter()
            .any(|tool| !tool.input_schema.is_null() || !tool.description.is_empty())
        {
            out.push(("gen_ai.tool.definitions", semconv_tools(&request.tools)));
        }
    }
    if let TraceEvent::ExecCodeCompleted { code, .. }
    | TraceEvent::ExecCodeFailed { code, .. }
    | TraceEvent::ExecCodeDiscarded { code, .. } = event
        && !code.is_empty()
    {
        out.push(("gen_ai.tool.call.arguments", json!({"code":code})));
    }
    match event {
        TraceEvent::TurnCompleted { input, output, .. } => {
            if !input.is_empty() {
                out.push(("gen_ai.input.messages", semconv_input(input)));
            }
            if !output.is_empty() {
                out.push(("gen_ai.output.messages", semconv_input(output)));
            }
        }
        TraceEvent::LlmCallCompleted { response, .. } => {
            let parts: Vec<_> = response
                .parts
                .iter()
                .flatten()
                .filter(|part| {
                    !matches!(part, LlmOutputPart::Reasoning { replay: Some(replay), .. }
                    if replay.redacted || replay.encrypted_content.is_some())
                })
                .map(|part| match part {
                    LlmOutputPart::Text { text, .. } => json!({"type":"text","content":text}),
                    LlmOutputPart::Reasoning { text, .. } => {
                        json!({"type":"reasoning","content":text})
                    }
                    LlmOutputPart::ToolCall {
                        call_id,
                        tool_name,
                        input_json,
                        ..
                    } => json!({
                        "type":"tool_call", "id":call_id, "name":tool_name,
                        "arguments":serde_json::from_str::<Value>(input_json)
                            .unwrap_or_else(|_| Value::String(input_json.clone()))
                    }),
                })
                .collect();
            let parts = if parts.is_empty() && !response.text.is_empty() {
                vec![json!({"type":"text","content":response.text})]
            } else {
                parts
            };
            if !parts.is_empty() {
                out.push(("gen_ai.output.messages",json!([{"role":"assistant","parts":parts,"finish_reason":response.terminal_reason.as_ref().map(|r|serde_json::to_value(r).unwrap_or(Value::Null)).unwrap_or(json!("unknown"))}])));
            }
        }
        TraceEvent::ToolCallCompleted { args, output, .. } => {
            if let crate::TraceToolArgs::Value(args) = args {
                out.push(("gen_ai.tool.call.arguments", object(args)));
            }
            match &output.outcome {
                TraceToolCallOutcome::Success(value) => {
                    out.push(("gen_ai.tool.call.result", object(value)))
                }
                TraceToolCallOutcome::Failure(value) | TraceToolCallOutcome::Cancelled(value) => {
                    out.push(("gen_ai.tool.call.result", json!({"error":value})))
                }
                _ => {}
            }
        }
        TraceEvent::ExecCodeCompleted { output, .. } if !output.is_empty() => {
            out.push(("gen_ai.tool.call.result", json!({"output":output})))
        }
        _ => {}
    }
    out
}
