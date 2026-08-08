use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

use lash_core::llm::types::LlmRequest;
use lash_core::provider::{ReasoningDisableEncoding, ReasoningEncoding, ReasoningSelection};
use lash_llm_transport::{LlmHttpTransport, ReqwestLlmHttpTransport};

use crate::reasoning::ReasoningWireIntent;

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
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 32_768;

pub(crate) fn reasoning_intent(req: &LlmRequest) -> Option<ReasoningWireIntent> {
    let reasoning = req.model_capability.reasoning.as_ref()?;
    match &req.model_variant {
        ReasoningSelection::ProviderDefault => None,
        ReasoningSelection::Effort(effort) => match &reasoning.encoding {
            ReasoningEncoding::Effort => Some(ReasoningWireIntent::Effort(effort.clone())),
            ReasoningEncoding::Budget(budgets) => budgets
                .get(effort)
                .copied()
                .map(ReasoningWireIntent::Budget),
        },
        ReasoningSelection::Disabled => match reasoning.disable.as_ref()? {
            ReasoningDisableEncoding::Native => {
                Some(ReasoningWireIntent::Effort("none".to_string()))
            }
            ReasoningDisableEncoding::Omit => None,
            ReasoningDisableEncoding::Effort(effort) => {
                Some(ReasoningWireIntent::Effort(effort.clone()))
            }
            ReasoningDisableEncoding::Budget(budget) => Some(ReasoningWireIntent::Budget(*budget)),
            ReasoningDisableEncoding::ToggleFalse => Some(ReasoningWireIntent::ToggleFalse),
        },
    }
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

pub(crate) fn empty_response_error(raw: String) -> lash_core::llm::transport::LlmTransportError {
    lash_core::llm::transport::LlmTransportError::new("OpenAI-compatible empty_response")
        .retryable(true)
        .with_code("empty_response")
        .with_raw(raw)
}

/// Which of the caller's generation options an assembled OpenAI-compatible
/// body carries.
///
/// The body is the evidence, so the record cannot drift from what was sent:
/// a compat profile that omits the token-cap field, a dialect without a seed
/// (Responses, Codex), and Codex declining both sampling controls all look the
/// same on the wire — the key is simply absent. None of these endpoints drops
/// a control because sampling is pinned; that is an Anthropic-only fact.
pub(crate) fn generation_disposition(
    request: &LlmRequest,
    body: &Value,
) -> lash_core::llm::types::GenerationDisposition {
    use lash_core::llm::types::{GenerationDisposition, GenerationOptionDisposition};

    fn record(requested: bool, emitted: bool) -> GenerationOptionDisposition {
        if emitted {
            GenerationOptionDisposition::applied(requested)
        } else {
            GenerationOptionDisposition::unsupported(requested)
        }
    }

    let cap_emitted = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .iter()
        .any(|field| body.get(field).is_some());
    GenerationDisposition {
        output_token_cap: record(request.generation.output_token_cap.is_some(), cap_emitted),
        temperature: record(
            request.generation.temperature.is_some(),
            body.get("temperature").is_some(),
        ),
        seed: record(
            request.generation.seed.is_some(),
            body.get("seed").is_some(),
        ),
        cache: lash_llm_transport::cache_intent_disposition(request, Some(body)),
    }
}

pub(crate) fn apply_max_tokens_field(
    body: &mut Value,
    field: crate::config::OpenAiCompatMaxTokensField,
    value: u64,
) {
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
        crate::config::OpenAiCompatMaxTokensField::Omit => {}
    }
}
