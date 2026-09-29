//! The OpenAI-compatible reasoning mapping: one resolved [`ReasoningIntent`]
//! onto the route's closed [`OpenAiReasoningDialect`], per endpoint.

use crate::config::OpenAiReasoningDialect;
use crate::driver::CompletionEndpoint;
use lash_core::llm::transport::{LlmTransportError, TransportRetryVerdict, TurnFailureCode};
use lash_core::provider::ReasoningIntent;
use serde_json::{Value, json};

/// Write `intent` onto `body` in the route's dialect, or refuse it.
///
/// A route with no dialect refuses every explicit intent: guessing a shape
/// would move the silent drop to a gateway that ignores unknown fields. The
/// `OpenAi` dialect has no token-budget field on either endpoint. Codex speaks
/// Responses in the `OpenAi` dialect.
pub(crate) fn apply_reasoning(
    endpoint: CompletionEndpoint,
    dialect: Option<OpenAiReasoningDialect>,
    intent: &ReasoningIntent,
    body: &mut Value,
) -> Result<(), LlmTransportError> {
    let Some(dialect) = dialect else {
        return Err(unrepresentable(
            "the route declares no reasoning dialect; set `OpenAiCompat.reasoning` or select `ProviderDefault`",
        ));
    };
    match dialect {
        OpenAiReasoningDialect::OpenAi => {
            // The OpenAI effort field names "off" as the effort `none`.
            let effort = match intent {
                ReasoningIntent::Effort(effort) => effort.as_str(),
                ReasoningIntent::Off => "none",
                ReasoningIntent::Budget(_) => {
                    return Err(unrepresentable(
                        "the OpenAI reasoning dialect has no token-budget field",
                    ));
                }
            };
            match endpoint {
                CompletionEndpoint::ChatCompletions => body["reasoning_effort"] = json!(effort),
                CompletionEndpoint::Responses => reasoning_object(body)["effort"] = json!(effort),
            }
        }
        OpenAiReasoningDialect::OpenRouter => {
            let reasoning = reasoning_object(body);
            match intent {
                ReasoningIntent::Effort(effort) => reasoning["effort"] = json!(effort),
                ReasoningIntent::Budget(max_tokens) => reasoning["max_tokens"] = json!(max_tokens),
                ReasoningIntent::Off => reasoning["enabled"] = json!(false),
            }
        }
    }
    Ok(())
}

/// The body's `reasoning` object, created empty when absent.
pub(crate) fn reasoning_object(body: &mut Value) -> &mut Value {
    if !body["reasoning"].is_object() {
        body["reasoning"] = json!({});
    }
    &mut body["reasoning"]
}

fn unrepresentable(detail: &str) -> LlmTransportError {
    LlmTransportError::new(format!("reasoning selection cannot be sent: {detail}"))
        .with_lash_code(TurnFailureCode::ReasoningEncodingUnrepresentable)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
}
