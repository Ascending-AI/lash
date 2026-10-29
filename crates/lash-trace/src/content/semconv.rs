use crate::{
    TraceAttachment, TraceContentBlock, TraceEvent, TraceLlmMessage, TraceLlmResponse,
    TraceToolCallOutcome, TraceToolResultBlock, TraceToolSpec,
};
use lash_protocol_types::llm::types::LlmOutputPart;
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;

// Keys follow the existing JSON Value map order, including null fields.
macro_rules! object {
    ($serializer:expr; $($key:literal => $value:expr),* $(,)?) => {{
        let mut map = $serializer.serialize_map(None)?;
        $(map.serialize_entry($key, &$value)?;)*
        map.end()
    }};
}

pub(super) struct Input<'a>(pub &'a [TraceLlmMessage]);
impl Serialize for Input<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(Message))
    }
}
struct Message<'a>(&'a TraceLlmMessage);
impl Serialize for Message<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        object!(s; "parts" => Blocks(&self.0.blocks), "role" => self.0.role)
    }
}
struct Blocks<'a>(&'a [TraceContentBlock]);
impl Serialize for Blocks<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(
            self.0
                .iter()
                .filter(|block| {
                    !matches!(
                        block,
                        TraceContentBlock::Reasoning {
                            has_encrypted: true,
                            ..
                        } | TraceContentBlock::Reasoning { redacted: true, .. }
                    )
                })
                .map(Block),
        )
    }
}
struct Attachment<'a>(&'a TraceAttachment);
impl Serialize for Attachment<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let a = self.0;
        object!(s; "byte_len" => a.byte_len, "delivery_form" => a.delivery_form,
            "lash.attachment.id" => a.id, "mime_type" => a.media_type,
            "type" => "lash.attachment")
    }
}
struct Text<'a>(&'a str, &'a str);
impl Serialize for Text<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        object!(s; "content" => self.1, "type" => self.0)
    }
}
struct Block<'a>(&'a TraceContentBlock);
impl Serialize for Block<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            TraceContentBlock::Text { text, .. } => Text("text", text).serialize(s),
            TraceContentBlock::Reasoning { text, .. } => Text("reasoning", text).serialize(s),
            TraceContentBlock::Attachment { reference } => Attachment(reference).serialize(s),
            TraceContentBlock::ToolCall {
                call_id,
                tool_name,
                input_json,
                ..
            } => {
                object!(s; "arguments" => input_json, "id" => call_id,
                    "name" => tool_name, "type" => "tool_call")
            }
            TraceContentBlock::ToolResult {
                call_id, content, ..
            } => {
                object!(s; "id" => call_id, "response" => Results(content),
                    "type" => "tool_call_response")
            }
        }
    }
}
struct Results<'a>(&'a [TraceToolResultBlock]);
impl Serialize for Results<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(ResultBlock))
    }
}
struct ResultBlock<'a>(&'a TraceToolResultBlock);
impl Serialize for ResultBlock<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            TraceToolResultBlock::Text { text } => Text("text", text).serialize(s),
            TraceToolResultBlock::Attachment { reference } => Attachment(reference).serialize(s),
        }
    }
}
pub(super) struct Tools<'a>(pub &'a [TraceToolSpec]);
impl Serialize for Tools<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(Tool))
    }
}
struct Tool<'a>(&'a TraceToolSpec);
impl Serialize for Tool<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        object!(s; "description" => self.0.description, "name" => self.0.name,
            "parameters" => self.0.input_schema, "type" => "function")
    }
}
fn visible(part: &&LlmOutputPart) -> bool {
    !matches!(part, LlmOutputPart::Reasoning { replay: Some(replay), .. }
        if replay.redacted || replay.encrypted_content.is_some())
}
struct ResponseParts<'a>(&'a TraceLlmResponse);
impl Serialize for ResponseParts<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let parts = self.0.parts.iter().flatten().filter(visible);
        if parts.clone().next().is_none() && !self.0.text.is_empty() {
            s.collect_seq([Text("text", &self.0.text)])
        } else {
            s.collect_seq(parts.map(OutputPart))
        }
    }
}
struct OutputPart<'a>(&'a LlmOutputPart);
impl Serialize for OutputPart<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            LlmOutputPart::Text { text, .. } => Text("text", text).serialize(s),
            LlmOutputPart::Reasoning { text, .. } => Text("reasoning", text).serialize(s),
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                ..
            } => {
                object!(s; "arguments" => Arguments(input_json), "id" => call_id,
                    "name" => tool_name, "type" => "tool_call")
            }
        }
    }
}
struct Arguments<'a>(&'a str);
impl Serialize for Arguments<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match serde_json::from_str::<Value>(self.0) {
            Ok(value) => value.serialize(s),
            Err(_) => self.0.serialize(s),
        }
    }
}
struct Response<'a>(&'a TraceLlmResponse);
impl Serialize for Response<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        object!(s; "finish_reason" => FinishReason(self.0),
            "parts" => ResponseParts(self.0), "role" => "assistant")
    }
}
struct FinishReason<'a>(&'a TraceLlmResponse);
impl Serialize for FinishReason<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match &self.0.terminal_reason {
            Some(reason) => reason.serialize(s),
            None => "unknown".serialize(s),
        }
    }
}

pub(crate) enum Attribute<'a> {
    System(&'a str),
    Input(&'a [TraceLlmMessage]),
    Tools(&'a [TraceToolSpec]),
    Response(&'a TraceLlmResponse),
    Object(&'a Value),
    Code(&'a str),
    Output(&'a str),
    Failure(&'a crate::TraceToolFailure),
    Cancelled(&'a crate::TraceToolCancellation),
}
impl Serialize for Attribute<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::System(text) => [Text("text", text)].serialize(s),
            Self::Input(messages) => Input(messages).serialize(s),
            Self::Tools(tools) => Tools(tools).serialize(s),
            Self::Response(response) => [Response(response)].serialize(s),
            Self::Object(value) if value.is_object() => value.serialize(s),
            Self::Object(value) => object!(s; "value" => value),
            Self::Code(code) => object!(s; "code" => code),
            Self::Output(output) => object!(s; "output" => output),
            Self::Failure(failure) => object!(s; "error" => Failure(failure)),
            Self::Cancelled(cancellation) => object!(s; "error" => Cancelled(cancellation)),
        }
    }
}
// Match Value's Display contract while streaming into the exporter's final
// string. Formatting errors propagate through fmt instead of adding a panic.
impl std::fmt::Display for Attribute<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        serde_json::to_writer(JsonFormatter(formatter), self).map_err(|_| std::fmt::Error)
    }
}
struct JsonFormatter<'a, 'b>(&'a mut std::fmt::Formatter<'b>);
impl std::io::Write for JsonFormatter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = std::str::from_utf8(bytes).map_err(std::io::Error::other)?;
        self.0.write_str(text).map_err(std::io::Error::other)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Borrowed views of the complete structured attributes, shared by bounds and export.
pub(crate) fn attributes(
    event: &TraceEvent,
) -> impl Iterator<Item = (&'static str, Attribute<'_>)> {
    let request = match event {
        TraceEvent::LlmCallCompleted { request, .. }
        | TraceEvent::LlmCallFailed { request, .. } => Some(request),
        _ => None,
    };
    let request = request.into_iter().flat_map(|request| {
        [
            request
                .system_instructions
                .as_deref()
                .map(|text| ("gen_ai.system_instructions", Attribute::System(text))),
            (!request.messages.is_empty())
                .then_some(("gen_ai.input.messages", Attribute::Input(&request.messages))),
            request
                .tools
                .iter()
                .any(|tool| !tool.input_schema.is_null() || !tool.description.is_empty())
                .then_some(("gen_ai.tool.definitions", Attribute::Tools(&request.tools))),
        ]
        .into_iter()
        .flatten()
    });
    let code = match event {
        TraceEvent::ExecCodeCompleted { code, .. }
        | TraceEvent::ExecCodeFailed { code, .. }
        | TraceEvent::ExecCodeDiscarded { code, .. }
            if !code.is_empty() =>
        {
            Some(("gen_ai.tool.call.arguments", Attribute::Code(code)))
        }
        _ => None,
    };
    let content = match event {
        TraceEvent::TurnCompleted { input, output, .. } => [
            (!input.is_empty()).then_some(("gen_ai.input.messages", Attribute::Input(input))),
            (!output.is_empty()).then_some(("gen_ai.output.messages", Attribute::Input(output))),
        ],
        TraceEvent::LlmCallCompleted { response, .. } => [
            (response.parts.iter().flatten().any(|part| visible(&part))
                || !response.text.is_empty())
            .then_some(("gen_ai.output.messages", Attribute::Response(response))),
            None,
        ],
        TraceEvent::ToolCallReturned { args, output, .. }
        | TraceEvent::ToolCallCompleted { args, output, .. } => [
            match args {
                crate::TraceToolArgs::Value(args) => {
                    Some(("gen_ai.tool.call.arguments", Attribute::Object(args)))
                }
                _ => None,
            },
            match &output.outcome {
                TraceToolCallOutcome::Success(value) => {
                    Some(("gen_ai.tool.call.result", Attribute::Object(value)))
                }
                TraceToolCallOutcome::Failure(failure) => {
                    Some(("gen_ai.tool.call.result", Attribute::Failure(failure)))
                }
                TraceToolCallOutcome::Cancelled(cancellation) => Some((
                    "gen_ai.tool.call.result",
                    Attribute::Cancelled(cancellation),
                )),
                TraceToolCallOutcome::Discarded => None,
            },
        ],
        TraceEvent::ExecCodeCompleted { output, .. } if !output.is_empty() => [
            Some(("gen_ai.tool.call.result", Attribute::Output(output))),
            None,
        ],
        _ => [None, None],
    };
    request.chain(code).chain(content.into_iter().flatten())
}

struct Failure<'a>(&'a crate::TraceToolFailure);
impl Serialize for Failure<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let crate::TraceToolFailure {
            bound: _,
            class,
            code,
            source,
            suggested_delay_ms,
            diagnostics,
        } = self.0;
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("class", class)?;
        map.serialize_entry("code", code)?;
        if let Some(diagnostics) = diagnostics {
            map.serialize_entry("diagnostics", diagnostics)?;
        }
        map.serialize_entry("source", source)?;
        if let Some(delay) = suggested_delay_ms {
            map.serialize_entry("suggested_delay_ms", delay)?;
        }
        map.end()
    }
}
struct Cancelled<'a>(&'a crate::TraceToolCancellation);
impl Serialize for Cancelled<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let crate::TraceToolCancellation {
            source,
            origin,
            forced,
            diagnostics,
        } = self.0;
        let mut map = s.serialize_map(None)?;
        if let Some(diagnostics) = diagnostics {
            map.serialize_entry("diagnostics", diagnostics)?;
        }
        if *forced {
            map.serialize_entry("forced", &true)?;
        }
        if let Some(origin) = origin {
            map.serialize_entry("origin", origin)?;
        }
        map.serialize_entry("source", source)?;
        map.end()
    }
}
