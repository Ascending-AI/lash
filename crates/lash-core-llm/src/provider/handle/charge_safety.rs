//! Which ground a failed attempt may be retried on, and the host's
//! charge-safety decision for the one ground that risks a duplicate charge.

use super::*;

/// The ground on which the transport's retryable verdict may be acted upon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::provider) enum RetryGround {
    /// The provider's guarantee or the failure's protocol position proves
    /// that another attempt cannot buy a second generation.
    Automatic(RetryClass),
    /// Nothing proves that: only the host's charge-safety policy may
    /// authorize the retry.
    Unguaranteed(UnguaranteedRetry),
}

impl RetryGround {
    /// `None` when the transport verdict bars a retry (`Forbidden` or
    /// `NotRetryable`): no guarantee and no host waiver overrides that.
    pub(in crate::provider) fn of(
        failure: &LlmTransportError,
        position: ProtocolPosition,
        guarantee: GenerationRetryGuarantee,
    ) -> Option<Self> {
        if !failure.is_retryable() {
            return None;
        }
        Some(match automatic_retry_class(failure, position, guarantee) {
            Some(class) => Self::Automatic(class),
            None => Self::Unguaranteed(UnguaranteedRetry {
                tokens_at_stake: failure
                    .partial_response
                    .as_deref()
                    .map(|response| duplicate_cost_tokens(&response.usage))
                    .unwrap_or_default(),
            }),
        })
    }
}

/// A retryable failure with no automatic retry class. [`RetryGround::of`] is
/// its only constructor, so a charge-safety decision exists only for a retry
/// that the transport permits and nothing proves charge-safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::provider) struct UnguaranteedRetry {
    tokens_at_stake: u64,
}

impl UnguaranteedRetry {
    /// `attempt_number` is the one-based unsafe retry this would be.
    pub(in crate::provider) fn decision(
        self,
        policy: &crate::ChargeSafetyPolicy,
        attempt_number: u8,
    ) -> ChargeSafetyDecision {
        let tokens_at_stake = self.tokens_at_stake;
        let denied = |reason| ChargeSafetyDecision::Denied {
            tokens_at_stake,
            attempt_number,
            reason,
        };
        match policy {
            crate::ChargeSafetyPolicy::RequireGuarantee => {
                denied(ChargeSafetyDenialReason::GuaranteeRequired)
            }
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries,
                max_duplicate_cost_tokens,
            } => {
                if attempt_number > *max_unsafe_retries {
                    return denied(ChargeSafetyDenialReason::UnsafeRetryLimitExceeded);
                }
                if max_duplicate_cost_tokens.is_some_and(|maximum| tokens_at_stake > maximum) {
                    return denied(ChargeSafetyDenialReason::DuplicateCostLimitExceeded);
                }
                ChargeSafetyDecision::Authorized {
                    tokens_at_stake,
                    attempt_number,
                }
            }
        }
    }
}

fn duplicate_cost_tokens(usage: &crate::llm::types::LlmUsage) -> u64 {
    let total = i128::from(usage.input_tokens)
        + i128::from(usage.output_tokens)
        + i128::from(usage.cache_read_input_tokens)
        + i128::from(usage.cache_write_input_tokens);
    total.clamp(0, i128::from(u64::MAX)) as u64
}
