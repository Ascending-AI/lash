use crate::dialect::TypescriptDialect;
use lash_core::llm::types::LlmToolSpec;
use lash_core::{LlmOutputPart, Part, PartKind};

/// The sole provider-native RLM tool. Termination remains inside its program.
pub const NATIVE_EXECUTE_TOOL_NAME: &str = "execute_code";

pub(super) fn tool_spec(dialect: &TypescriptDialect) -> LlmToolSpec {
    let definition = lash_core::ToolDefinition::raw(
        "rlm:execute_code",
        NATIVE_EXECUTE_TOOL_NAME,
        format!(
            "Execute {} in the persistent session",
            dialect.language_id()
        ),
        serde_json::json!({"type":"object","properties":{"code":{"type":"string","description":format!("{} program to execute in the persistent session", dialect.language_id())}},"required":["code"],"additionalProperties":false}),
        serde_json::json!({"type":"string"}),
    );
    let contract = definition.contract();
    LlmToolSpec {
        name: NATIVE_EXECUTE_TOOL_NAME.to_string(),
        description: format!(
            "Execute {} in the persistent session",
            dialect.language_id()
        ),
        input_schema: contract.input_schema,
        output_schema: contract.output_schema,
    }
}

pub(super) enum NativeAction {
    Execute {
        code: String,
    },
    ProseOnly,
    Malformed {
        decision: &'static str,
        repair_copy: String,
    },
}

pub(super) fn normalize(parts: &[Part]) -> NativeAction {
    let calls = parts
        .iter()
        .filter(|p| p.kind() == PartKind::ToolCall)
        .map(|call| (call.tool_name(), call.content().to_string()))
        .collect::<Vec<_>>();
    normalize_calls(&calls)
}

/// [`normalize`] over a response's output parts, before they are named.
pub(super) fn normalize_output(parts: &[LlmOutputPart]) -> NativeAction {
    let calls = parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::ToolCall {
                tool_name,
                input_json,
                ..
            } => Some((Some(tool_name.as_str()), input_json.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    normalize_calls(&calls)
}

fn normalize_calls(calls: &[(Option<&str>, String)]) -> NativeAction {
    let malformed = |decision, copy: &str| NativeAction::Malformed {
        decision,
        repair_copy: copy.to_string(),
    };
    if calls.len() > 1 {
        return malformed(
            "retry_multiple_calls",
            "No code executed: only one execute_code call is allowed per response. Combine the work into one program.",
        );
    }
    let Some((tool_name, content)) = calls.first() else {
        return NativeAction::ProseOnly;
    };
    if *tool_name != Some(NATIVE_EXECUTE_TOOL_NAME) {
        return malformed(
            "retry_unknown_tool",
            "No code executed: unknown tool. Call execute_code; invoke host operations and finish inside code.",
        );
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return malformed(
            "retry_invalid_arguments",
            "No code executed: arguments must be valid JSON with exactly one string property, code.",
        );
    };
    let Some(object) = value.as_object() else {
        return malformed(
            "retry_invalid_arguments",
            "No code executed: arguments must be an object with exactly one string property, code.",
        );
    };
    let Some(code) = object
        .get("code")
        .and_then(serde_json::Value::as_str)
        .filter(|code| !code.trim().is_empty())
    else {
        return malformed(
            "retry_missing_code",
            "No code executed: code must be a nonempty string containing the program.",
        );
    };
    if object.len() != 1 {
        return malformed(
            "retry_invalid_arguments",
            "No code executed: additional argument properties are forbidden; send only code.",
        );
    }
    NativeAction::Execute {
        code: code.to_string(),
    }
}

/// The response's parts as transcript parts, its `n`th tool call named
/// `call_ids[n]` (the response's own positions, ADR 0117 §2).
pub(super) fn assistant_parts(
    parts: Vec<LlmOutputPart>,
    call_ids: Vec<lash_core::ToolCallId>,
) -> Vec<Part> {
    let mut call_ids = call_ids.into_iter();
    parts
        .into_iter()
        .enumerate()
        .filter_map(|(index, part)| {
            let id = format!("native.p{index}");
            match part {
                LlmOutputPart::Text {
                    text,
                    response_meta,
                } => Some(Part::prose(id, text, response_meta)),
                LlmOutputPart::Reasoning { text, replay } => {
                    Some(Part::reasoning(id, text, replay))
                }
                LlmOutputPart::ToolCall {
                    call_id,
                    tool_name,
                    input_json,
                    replay,
                } => Some(Part::tool_call(
                    id,
                    input_json,
                    call_ids.next()?,
                    call_id,
                    tool_name,
                    replay,
                )),
            }
        })
        .collect()
}
