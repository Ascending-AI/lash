use crate::llm::types::AttachmentSource;
use crate::{AttachmentRef, ToolCallRecord};

/// One source-level dispatch, optionally joined to the host tool record it produced.
///
/// `operation` and `outcome` are the model-safe execution ledger. Host records
/// are attached only when the source dispatch resolved to a host tool call;
/// trigger and other host-internal dispatches therefore carry `None`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExecutedCall {
    pub operation: String,
    pub outcome: ExecutedCallOutcome,
    pub host_record: Option<ToolCallRecord>,
}

/// Compact source-level record of an effect the embedded executor actually ran.
///
/// Unlike [`ToolCallRecord`], this deliberately carries neither arguments nor
/// host-operation details. Protocols can safely replay it to a model as an
/// execution ledger without exposing inputs or confusing a source module call
/// with the host tool it resolved to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutedCallRecord {
    pub operation: String,
    pub outcome: ExecutedCallOutcome,
}

/// Typed accounting for host tool records omitted from a bounded turn view.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OmittedToolCalls {
    pub count: usize,
    pub failures: usize,
    pub attachments: Vec<AttachmentSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutedCallOutcome {
    Ok,
    Err,
}

impl ExecutedCallOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Err => "err",
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TextProjectionMetadata {
    pub truncated: bool,
    pub original_chars: usize,
    pub projected_chars: usize,
    pub original_lines: usize,
    pub projected_lines: usize,
    pub limit: usize,
    pub limit_mode: String,
    pub max_lines: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DegradedBinding {
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Observation {
    pub text: String,
    pub projection: TextProjectionMetadata,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExecResponse {
    pub observations: Vec<Observation>,
    pub calls: Vec<ExecutedCall>,
    pub printed_images: Vec<AttachmentRef>,
    pub error: Option<String>,
    pub duration_ms: u64,
    /// Bindings that could not be restored to a live host reference during
    /// executor setup. The executor leaves each binding loudly unavailable;
    /// the host decides whether to warn, repair, or abort.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_bindings: Vec<DegradedBinding>,
    /// When the surrounding session uses protocol-specific finish behavior,
    /// this carries the protocol's terminal value. The dispatch loop uses it
    /// as the terminal result of the session. `None` for chat-style sessions
    /// and for typed sessions whose step continued without finishing.
    pub terminal_finish: Option<serde_json::Value>,
}

/// Exact prompt-usage snapshot from the most recent completed LLM call.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PromptUsage {
    pub prompt_context_tokens: usize,
    pub input_tokens: usize,
    pub cache_read_input_tokens: usize,
    pub cache_write_input_tokens: usize,
    pub context_budget_tokens: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_exec_response_payload_with_images_field_still_decodes() {
        let legacy_json = serde_json::json!({
            "observations": [{
                "text": "step output",
                "projection": {
                    "truncated": false,
                    "original_chars": 11,
                    "projected_chars": 11,
                    "original_lines": 1,
                    "projected_lines": 1,
                    "limit": 51200,
                    "limit_mode": "bytes",
                    "max_lines": 2000
                }
            }],
            "calls": [],
            "images": [
                {
                    "mime": "image/png",
                    "label": "legacy_image",
                    "data": [1, 2, 3]
                }
            ],
            "printed_images": [],
            "error": null,
            "duration_ms": 42,
            "terminal_finish": null
        });

        let response: ExecResponse = serde_json::from_value(legacy_json)
            .expect("legacy ExecResponse payload with images field should decode");
        assert_eq!(response.observations[0].text, "step output");
        assert_eq!(response.duration_ms, 42);
    }

    #[test]
    fn paired_observation_lists_are_rejected() {
        let mut paired_json = serde_json::json!({
            "observations": ["step output"],
            "calls": [],
            "printed_images": [],
            "error": null,
            "duration_ms": 42,
            "terminal_finish": null
        });
        paired_json.as_object_mut().unwrap().insert(
            "observation_truncation".to_string(),
            serde_json::json!([{
                "truncated": false,
                "original_chars": 11,
                "projected_chars": 11,
                "original_lines": 1,
                "projected_lines": 1,
                "limit": 51200,
                "limit_mode": "bytes",
                "max_lines": 2000
            }]),
        );

        let error = serde_json::from_value::<ExecResponse>(paired_json)
            .expect_err("paired observation lists must not decode after the hard cutover");
        assert!(
            error.to_string().contains("expected struct Observation"),
            "unexpected decode error: {error}"
        );
    }

    #[test]
    fn two_list_exec_response_payload_is_rejected() {
        let legacy_json = serde_json::json!({
            "observations": [],
            "tool_calls": [],
            "executed_calls": [],
            "printed_images": [],
            "error": null,
            "duration_ms": 42,
            "degraded_bindings": [],
            "terminal_finish": null
        });

        let error = serde_json::from_value::<ExecResponse>(legacy_json)
            .expect_err("the pre-cutover two-list ExecResponse must be refused");
        assert!(
            error.to_string().contains("missing field `calls`"),
            "unexpected decode error: {error}"
        );
    }
}
