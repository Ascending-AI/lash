//! Anthropic wire constants and the reasoning mapping.
//!
//! Model capability (which efforts a model exposes and how they map onto the
//! wire) is host-supplied data threaded onto every [`LlmRequest`] and resolved
//! once into a [`ReasoningIntent`]. This crate never sniffs model names to
//! infer capability; it only maps that intent onto the Anthropic
//! `thinking`/`output_config` wire shape.

use crate::support::*;
use lash_core::provider::ReasoningIntent;

pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
pub(crate) const FINE_GRAINED_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
pub(crate) const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
pub(crate) const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// The one Anthropic reasoning mapping: a resolved [`ReasoningIntent`] onto
/// `thinking`/`output_config`, or a refusal.
///
/// An effort is adaptive thinking with `output_config.effort`, a budget is
/// budget thinking, and off is `thinking: disabled`. `display` requests the
/// summary only while thinking is active. A budget must stay below the cap
/// Anthropic requires, or the call is refused before any I/O.
pub(crate) fn apply_thinking(
    intent: &ReasoningIntent,
    expose_thinking: bool,
    max_tokens: u64,
    body: &mut Value,
) -> Result<(), LlmTransportError> {
    let display = if expose_thinking {
        "summarized"
    } else {
        "omitted"
    };
    match intent {
        ReasoningIntent::Effort(effort) => {
            body["thinking"] = json!({
                "type": "adaptive",
                "display": display,
            });
            if !body.get("output_config").is_some_and(Value::is_object) {
                body["output_config"] = json!({});
            }
            body["output_config"]["effort"] = json!(effort);
        }
        ReasoningIntent::Budget(budget_tokens) => {
            if u64::from(*budget_tokens) >= max_tokens {
                return Err(LlmTransportError::new(format!(
                    "Anthropic Messages needs a thinking budget below `max_tokens`; the selected budget of {budget_tokens} tokens does not fit under the cap of {max_tokens}."
                ))
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::ReasoningBudgetExceedsOutputCap)
                .with_retry_verdict(TransportRetryVerdict::Forbidden));
            }
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget_tokens,
                "display": display,
            });
        }
        ReasoningIntent::Off => {
            body["thinking"] = json!({ "type": "disabled" });
        }
    }
    Ok(())
}
