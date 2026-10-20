use super::{ContentCategory as C, ContentMarks};
use crate::{ContentLimits, TraceEvent};
use serde::Serialize;
use serde_json::Value;

pub(super) fn size(value: &impl Serialize) -> u64 {
    lash_protocol_types::core_support::json_encoded_len(value).map_or(u64::MAX, |v| v as u64)
}
pub(super) fn cut(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}
pub(super) fn leaves(
    value: &mut Value,
    limit: usize,
    all_strings: bool,
    path: &str,
    marks: &mut ContentMarks,
) {
    match value {
        Value::String(text) if all_strings => {
            if cut(text, limit) {
                marks.truncated.push(path.to_owned());
            }
        }
        Value::Array(values) => {
            for (i, value) in values.iter_mut().enumerate() {
                leaves(value, limit, all_strings, &format!("{path}[{i}]"), marks);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                leaves(
                    value,
                    limit,
                    all_strings || matches!(key.as_str(), "text" | "content" | "summary"),
                    &format!("{path}.{key}"),
                    marks,
                );
            }
        }
        _ => {}
    }
}
impl TraceEvent {
    pub(crate) fn truncate_content(&mut self, limits: ContentLimits, marks: &mut ContentMarks) {
        let part = limits.max_part_bytes() as usize;
        let attribute = limits.max_attribute_bytes();
        self.visit_content(&mut |field| {
            let category = field.category();
            let path = field.path().clone();
            if matches!(
                category,
                C::ToolDefinitions | C::OutputSchema | C::ProviderPayloads | C::ResponseMetadata
            ) {
                return;
            }
            match &mut field.value {
                super::FieldValue::Text(text) => {
                    if cut(text, part) {
                        marks.truncated.push(path);
                    }
                }
                super::FieldValue::Json(value) => value.truncate(
                    part,
                    matches!(category, C::ToolArguments | C::ToolResults | C::Extension),
                    &path,
                    marks,
                ),
            }
        });
        if let Some(request) = self.request_mut() {
            while request.messages.len() > 1
                && (size(&request.messages) > attribute
                    || size(&super::semconv_input(&request.messages)) > attribute)
            {
                request.messages.remove(0);
                marks.dropped_messages += 1;
            }
        }
        if let Self::TurnCompleted { input, .. } = self {
            while input.len() > 1 && size(&super::semconv_input(input)) > attribute {
                input.remove(0);
                marks.dropped_messages += 1;
            }
        }
        self.visit_content(&mut |field| {
            let bytes = match &mut field.value {
                super::FieldValue::Text(value) => size(value),
                super::FieldValue::Json(value) => value.encoded_len(),
            };
            if bytes > attribute {
                marks.oversize.push(field.path().clone());
                field.omit();
            }
        });
        if let Some(request) = self.request_mut()
            && size(&request.messages) > attribute
        {
            request.messages.clear();
            marks.oversize.push("input_messages".into());
        }
        if let Self::LlmCallCompleted { response, .. } = self
            && size(&response.parts) > attribute
        {
            response.parts = None;
            response.text.clear();
            marks.oversize.push("output_messages.parts".into());
        }
        // Lists and schema wrappers can exceed the bound while every leaf fits.
        if let Some(request) = self.request_mut()
            && (size(&request.tools) > attribute
                || size(&super::semconv_tools(&request.tools)) > attribute)
        {
            for tool in &mut request.tools {
                tool.description.clear();
                tool.input_schema = Value::Null;
                tool.output_schema = Value::Null;
            }
            marks.oversize.push("tool_definitions".into());
        }
        // The actual semconv attribute includes wrappers. Clear the owning field
        // whenever that final representation is too large; never slice JSON text.
        let oversized: Vec<_> = super::attributes(self)
            .filter(|(_, value)| size(value) > attribute)
            .map(|(key, _)| key)
            .collect();
        for key in oversized {
            match key {
                "gen_ai.system_instructions" => {
                    if let Some(req) = self.request_mut() {
                        req.system_instructions = None;
                    }
                }
                "gen_ai.input.messages" => {
                    if let Some(req) = self.request_mut() {
                        req.messages.clear();
                    }
                    if let Self::TurnCompleted { input, .. } = self {
                        input.clear();
                    }
                }
                "gen_ai.tool.definitions" => {
                    if let Some(req) = self.request_mut() {
                        for tool in &mut req.tools {
                            tool.description.clear();
                            tool.input_schema = Value::Null;
                            tool.output_schema = Value::Null;
                        }
                    }
                }
                "gen_ai.output.messages" => {
                    if let Self::LlmCallCompleted { response, .. } = self {
                        response.text.clear();
                        response.parts = None;
                    }
                    if let Self::TurnCompleted { output, .. } = self {
                        output.clear();
                    }
                }
                "gen_ai.tool.call.arguments" => match self {
                    Self::ToolCallCompleted {
                        args: crate::TraceToolArgs::Value(args),
                        ..
                    }
                    | Self::ToolCallStarted { args, .. } => *args = Value::Null,
                    Self::ExecCodeCompleted { code, .. }
                    | Self::ExecCodeFailed { code, .. }
                    | Self::ExecCodeDiscarded { code, .. } => code.clear(),
                    _ => {}
                },
                "gen_ai.tool.call.result" => match self {
                    Self::ToolCallCompleted { .. } => self.visit_content(&mut |field| {
                        if field.category() == C::ToolResults {
                            field.omit();
                        }
                    }),
                    Self::ExecCodeCompleted { output, .. } => output.clear(),
                    _ => {}
                },
                _ => {}
            }
            marks.oversize.push(key.into());
        }
    }
}
