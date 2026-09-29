use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

use lash_core::llm::transport::{
    LlmTransportError, ProviderFailureKind, TransportRetryVerdict, TurnFailureCode,
};
use lash_core::llm::types::ReasoningRetentionValidationError;
use lash_llm_transport::{LlmHttpTransport, ReqwestLlmHttpTransport};

pub(crate) use lash_llm_transport::{
    merge_usage,
    openai_terminal_reason_from_chat_finish_reason as terminal_reason_from_chat_finish_reason,
    openai_terminal_reason_from_chat_value as terminal_reason_from_chat_value,
    openai_terminal_reason_from_response_value as terminal_reason_from_responses_value,
    openai_usage_from_response_value as usage_from_response_value,
    openai_usage_from_usage_value as usage_from_usage_value, terminal_reason_from_parts,
};

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// A request body and the receipt of the host settings it carries, built
/// together so the receipt comes from what each branch emitted rather than
/// from reading the body back.
#[derive(Clone, Debug)]
pub(crate) struct BuiltRequest {
    pub(crate) body: Value,
    pub(crate) receipt: lash_core::llm::types::GenerationReceipt,
}

pub(crate) fn reasoning_retention_transport_error(
    error: ReasoningRetentionValidationError,
) -> LlmTransportError {
    LlmTransportError::new(error.message)
        .with_kind(ProviderFailureKind::Unsupported)
        .with_lash_code(TurnFailureCode::UnsupportedReasoningRetention)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
}

pub(crate) static DEFAULT_HTTP_TRANSPORT: LazyLock<Arc<dyn LlmHttpTransport>> =
    LazyLock::new(|| Arc::new(ReqwestLlmHttpTransport::new()));

pub(crate) fn has_response_content(parts: &[lash_core::llm::types::LlmOutputPart]) -> bool {
    parts.iter().any(|part| match part {
        lash_core::llm::types::LlmOutputPart::Text { text, .. } => !text.is_empty(),
        lash_core::llm::types::LlmOutputPart::Reasoning { .. } => true,
        lash_core::llm::types::LlmOutputPart::ToolCall { .. } => true,
    })
}

/// Whether a contentless response is still missing the evidence needed to
/// classify it. A normal stop is valid only when the wire carried an explicit
/// successful terminal status; compatibility EOF tolerance must not manufacture success
/// from an empty, unterminated stream. The other terminal outcomes already
/// carry their own distinct semantics even when they contain no output.
pub(crate) fn invalid_empty_response(
    parts: &[lash_core::llm::types::LlmOutputPart],
    terminal_reason: lash_core::llm::types::LlmTerminalReason,
    normal_completion_seen: bool,
) -> bool {
    if has_response_content(parts) {
        return false;
    }
    match terminal_reason {
        lash_core::llm::types::LlmTerminalReason::Stop => !normal_completion_seen,
        lash_core::llm::types::LlmTerminalReason::OutputLimit
        | lash_core::llm::types::LlmTerminalReason::ContentFilter
        | lash_core::llm::types::LlmTerminalReason::Cancelled => false,
        _ => true,
    }
}

pub(crate) fn empty_response_error(raw: String) -> lash_core::llm::transport::LlmTransportError {
    empty_response_diagnostic(crate::request_work::body_excerpt(&raw))
}

pub(crate) fn empty_response_diagnostic(
    raw: String,
) -> lash_core::llm::transport::LlmTransportError {
    lash_core::llm::transport::LlmTransportError::new("OpenAI-compatible empty_response")
        .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::NotRetryable)
        .with_lash_code(TurnFailureCode::EmptyResponse)
        .with_raw(raw)
}

/// Write the cap in the endpoint's field and report whether one was written.
/// `Omit` endpoints never get here with a cap: resolution refuses it.
pub(crate) fn apply_max_tokens_field(
    body: &mut Value,
    field: crate::config::OpenAiCompatMaxTokensField,
    value: Option<u64>,
) -> bool {
    let Some(value) = value else {
        return false;
    };
    match field {
        crate::config::OpenAiCompatMaxTokensField::MaxTokens => {
            body["max_tokens"] = json!(value);
        }
        crate::config::OpenAiCompatMaxTokensField::MaxCompletionTokens => {
            body["max_completion_tokens"] = json!(value);
        }
        crate::config::OpenAiCompatMaxTokensField::MaxOutputTokens => {
            body["max_output_tokens"] = json!(value);
        }
        crate::config::OpenAiCompatMaxTokensField::Omit => return false,
    }
    true
}

/// How an OpenAI-compatible endpoint's cap field reads to resolution.
pub(crate) fn output_cap_wire(
    field: crate::config::OpenAiCompatMaxTokensField,
) -> lash_core::provider::OutputCapWire {
    match field {
        crate::config::OpenAiCompatMaxTokensField::Omit => {
            lash_core::provider::OutputCapWire::Unsupported
        }
        _ => lash_core::provider::OutputCapWire::Optional,
    }
}
