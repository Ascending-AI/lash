use crate::runtime::DecodedEffectOutcome as _;
use crate::runtime::effect::token_usage_from_llm;
use crate::{LlmResponse, PluginError, RuntimeEffectOutcome, TokenUsage};

// =============================================================================
// Direct-completion outcome plumbing
// =============================================================================

/// Both the text-only (`DirectCompletion`) and full-output (`DirectLlmCompletion`) client
/// methods project from this single result.
///
/// The usage it reports is the response's, for the caller; the ledger was
/// written by the effect's usage run when the effect was recorded (ADR 0125).
/// The call's trace records were made by the effect's body when it ran: this
/// projection runs on every replay and reports nothing.
pub(crate) fn apply_direct_outcome(
    outcome: RuntimeEffectOutcome,
) -> Result<(LlmResponse, TokenUsage, crate::LlmCallRecord), PluginError> {
    let (result, call_record) = outcome
        .into_direct_response()
        .map_err(|err| PluginError::Session(err.to_string()))?;
    let (response, usage) = match result {
        Ok(response) => {
            let usage = token_usage_from_llm(&response.usage);
            (response, usage)
        }
        Err(err) => {
            return if err.code.as_ref().and_then(crate::FailureCode::turn_code)
                == Some(crate::TurnFailureCode::UsageOwnerRetired)
            {
                Err(PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::UsageOwnerRetired,
                    err.message,
                )))
            } else {
                Err(PluginError::Session(err.message))
            };
        }
    };
    let call_record = call_record.ok_or_else(|| {
        PluginError::Session(
            "direct LLM effect completed without a provider call record".to_string(),
        )
    })?;
    Ok((response, usage, call_record))
}
