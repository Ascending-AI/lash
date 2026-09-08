use crate::dialect::RlmDialect;
use lash_core::llm::types::LlmToolSpec;
use lash_core::{LlmOutputPart, Part, PartKind};

/// The sole provider-native RLM tool. Termination remains inside its program.
pub const NATIVE_EXECUTE_TOOL_NAME: &str = "execute_code";

pub(super) fn tool_spec(dialect: &dyn RlmDialect) -> LlmToolSpec {
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
        .filter(|p| p.kind == PartKind::ToolCall)
        .collect::<Vec<_>>();
    let malformed = |decision, copy: &str| NativeAction::Malformed {
        decision,
        repair_copy: copy.to_string(),
    };
    let mut ids = std::collections::HashSet::new();
    if calls.iter().any(|p| !ids.insert(p.tool_call_id.as_deref())) {
        return malformed(
            "retry_duplicate_call_id",
            "No code executed: duplicate call ids. Send exactly one execute_code call with a unique id.",
        );
    }
    if calls.len() > 1 {
        return malformed(
            "retry_multiple_calls",
            "No code executed: only one execute_code call is allowed per response. Combine the work into one program.",
        );
    }
    let Some(call) = calls.first() else {
        return NativeAction::ProseOnly;
    };
    if call.tool_name.as_deref() != Some(NATIVE_EXECUTE_TOOL_NAME) {
        return malformed(
            "retry_unknown_tool",
            "No code executed: unknown tool. Call execute_code; invoke host operations and finish inside code.",
        );
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&call.content) else {
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

pub(super) fn assistant_parts(parts: Vec<LlmOutputPart>) -> Vec<Part> {
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let id = format!("native.p{index}");
            match part {
                LlmOutputPart::Text {
                    text,
                    response_meta,
                } => Part::prose(id, text, response_meta),
                LlmOutputPart::Reasoning { text, replay } => Part::reasoning(id, text, replay),
                LlmOutputPart::ToolCall {
                    call_id,
                    tool_name,
                    input_json,
                    replay,
                } => Part::tool_call(id, input_json, call_id, tool_name, replay),
            }
        })
        .collect()
}
