use crate::{ContentCategory as C, TraceRecord};
use opentelemetry::{Array, KeyValue, StringValue, Value};
pub(super) fn attributes(record: &TraceRecord, out: &mut Vec<KeyValue>) {
    for (key, value) in crate::content::attributes(&record.event) {
        let category = match key {
            "gen_ai.system_instructions" => C::SystemInstructions,
            "gen_ai.input.messages" => C::InputMessages,
            "gen_ai.output.messages" => C::OutputMessages,
            "gen_ai.tool.definitions" => C::ToolDefinitions,
            "gen_ai.tool.call.arguments" => {
                if matches!(
                    record.event,
                    crate::TraceEvent::ExecCodeCompleted { .. }
                        | crate::TraceEvent::ExecCodeFailed { .. }
                        | crate::TraceEvent::ExecCodeDiscarded { .. }
                ) {
                    C::Code
                } else {
                    C::ToolArguments
                }
            }
            "gen_ai.tool.call.result" => {
                if matches!(record.event, crate::TraceEvent::ExecCodeCompleted { .. }) {
                    C::CodeOutput
                } else {
                    C::ToolResults
                }
            }
            _ => continue,
        };
        let path = match category {
            C::SystemInstructions => "system_instructions",
            C::InputMessages => "input_messages",
            C::OutputMessages => "output_messages.parts",
            C::ToolDefinitions => "tool_definitions",
            C::ToolArguments => "tool_arguments",
            C::ToolResults => "tool_results",
            C::Code => "code",
            C::CodeOutput => "code_output",
            _ => "",
        };
        if record.content.captured.contains(&category)
            && !record
                .content
                .oversize
                .iter()
                .chain(&record.content.omitted)
                .any(|p| p == key || p == path)
        {
            out.push(KeyValue::new(key, value.to_string()));
        }
    }
    for (key, paths) in [
        ("lash.content.truncated", &record.content.truncated),
        ("lash.content.oversize", &record.content.oversize),
    ] {
        if !paths.is_empty() {
            out.push(KeyValue::new(
                key,
                Value::Array(Array::String(
                    paths.iter().cloned().map(StringValue::from).collect(),
                )),
            ));
        }
    }
    if record.content.dropped_messages > 0 {
        out.push(KeyValue::new(
            "lash.content.dropped_messages",
            i64::try_from(record.content.dropped_messages).unwrap_or(i64::MAX),
        ));
    }
    if let crate::TraceEvent::ToolCallCompleted { args, output, .. } = &record.event {
        if out
            .iter()
            .any(|a| a.key.as_str() == "gen_ai.tool.call.arguments")
            && matches!(args, crate::TraceToolArgs::Value(value) if !value.is_object())
        {
            out.push(KeyValue::new("lash.tool.arguments.wrapped", true));
        }
        if out
            .iter()
            .any(|a| a.key.as_str() == "gen_ai.tool.call.result")
            && let crate::TraceToolCallOutcome::Success(value) = &output.outcome
            && !value.is_object()
        {
            out.push(KeyValue::new("lash.tool.result.wrapped", true));
        }
    }
}
