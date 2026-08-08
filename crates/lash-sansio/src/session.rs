use crate::{AttachmentRef, MediaType, ToolCallRecord};

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

/// An image emitted by a trajectory step.
///
/// Printed images deliberately live outside the provider-attachment source
/// seams, while sharing their validated [`MediaType`] vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecImage {
    pub mime: MediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<AttachmentRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<u8>,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExecResponse {
    pub observations: Vec<String>,
    pub observation_truncation: Vec<TextProjectionMetadata>,
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default)]
    pub executed_calls: Vec<ExecutedCallRecord>,
    pub images: Vec<ExecImage>,
    pub printed_images: Vec<AttachmentRef>,
    pub error: Option<String>,
    pub duration_ms: u64,
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
    fn exec_image_media_type_round_trips_as_the_existing_string_wire_shape() {
        let image = ExecImage {
            mime: MediaType::parse("image/webp").unwrap(),
            reference: None,
            data: vec![1, 2, 3],
            label: "plot".to_string(),
            width: Some(320),
            height: Some(180),
        };

        let value = serde_json::to_value(&image).unwrap();
        assert_eq!(value["mime"], "image/webp");
        assert_eq!(serde_json::from_value::<ExecImage>(value).unwrap(), image);
    }

    #[test]
    fn exec_image_rejects_an_invalid_media_type_on_deserialization() {
        let error = serde_json::from_value::<ExecImage>(serde_json::json!({
            "mime": "not-a-mime",
            "data": [],
            "label": "plot"
        }))
        .unwrap_err();

        assert!(error.to_string().contains("invalid media type"));
    }
}
