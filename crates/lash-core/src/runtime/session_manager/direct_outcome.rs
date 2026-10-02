use crate::runtime::DecodedEffectOutcome as _;
use crate::runtime::effect::token_usage_from_llm;
use crate::{LlmResponse, PluginError, RuntimeEffectOutcome, TokenUsage};

// =============================================================================
// Direct-completion outcome plumbing
// =============================================================================

/// Both the text-only (`DirectCompletion`) and full-output (`DirectLlmCompletion`) client
/// methods project from this single result.
///
/// The usage it reports is the response's, retained with the model result
/// and attempt history in the effect journal (ADR 0127).
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
            return Err(PluginError::ProviderFailure {
                kind: err.kind,
                code: err.code,
                retryable: err.retryable,
                terminal_reason: err.terminal_reason,
                message: err.message,
            });
        }
    };
    let call_record = call_record.ok_or_else(|| {
        PluginError::Session(
            "direct LLM effect completed without a provider call record".to_string(),
        )
    })?;
    Ok((response, usage, call_record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_quota_refusal_retains_its_typed_provider_cause() {
        let error = crate::LlmCallError {
            message: "host spend cap reached".to_string(),
            retryable: false,
            kind: crate::ProviderFailureKind::Quota,
            raw: None,
            code: Some(crate::FailureCode::provider("host_spend_cap")),
            terminal_reason: crate::LlmTerminalReason::ProviderError,
            request_body: None,
            partial_response: None,
        };
        let refusal = apply_direct_outcome(RuntimeEffectOutcome::Direct {
            result: Box::new(Err(error)),
            call_record: None,
        })
        .expect_err("the host refused before spending");
        let live = serde_json::to_value(&refusal).expect("encode live cause");
        let recorded = serde_json::to_value(crate::ToolIntentCommandFailure::from(&refusal))
            .expect("encode recorded cause");
        let runtime = refusal
            .clone()
            .into_turn_failure(crate::RuntimeErrorCode::Plugin);
        assert_eq!(runtime.code, crate::RuntimeErrorCode::LlmProvider);
        assert!(runtime.is_terminal());
        assert!(
            matches!(runtime.cause, Some(crate::RuntimeErrorCause::ProviderFailure {
            failure_kind: crate::ProviderFailureKind::Quota,
            retryable: false,
            code: Some(ref code), ..
        }) if code == &crate::FailureCode::provider("host_spend_cap"))
        );
        for cause in [live, recorded] {
            assert_eq!(cause["type"], "provider_failure");
            assert_eq!(cause["message"]["kind"], "quota");
            assert_eq!(cause["message"]["code"], "provider:host_spend_cap");
            assert_eq!(cause["message"]["retryable"], false);
        }
    }
}
