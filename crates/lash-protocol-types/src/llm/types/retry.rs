use super::{ChargeSafetyDecision, ChargeSafetyDenialReason};
use schemars::JsonSchema;

/// The sealed decision for a prospective provider retry.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum RetryDecision {
    Scheduled {
        delay: std::time::Duration,
        wait: RetryWait,
        class: RetryClass,
    },
    Declined(RetryDeclineCause),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetryWait {
    Throttle,
    Backoff,
}

/// Evidence that permits another provider attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub enum RetryClass {
    NoResponse,
    RejectedHttpResponse,
    EmptyStreamPartial,
    ProviderIdempotency,
    ProviderResume,
    ChargeAuthorized {
        tokens_at_stake: u64,
        attempt_number: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(tag = "cause", rename_all = "snake_case", deny_unknown_fields)]
pub enum RetryDeclineCause {
    NotRetryable,
    RetryBudgetExhausted,
    RetryAfterExceedsCap,
    /// The call's limit expired: the model total ends every further attempt.
    TimedOut {
        limit: crate::LimitCause,
    },
    ChargeSafety {
        tokens_at_stake: u64,
        attempt_number: u8,
        reason: ChargeSafetyDenialReason,
    },
}

impl RetryDecision {
    pub fn is_scheduled(&self) -> bool {
        matches!(self, Self::Scheduled { .. })
    }

    pub fn delay(&self) -> Option<std::time::Duration> {
        match self {
            Self::Scheduled { delay, .. } => Some(*delay),
            Self::Declined(_) => None,
        }
    }

    pub fn decline_cause(&self) -> Option<RetryDeclineCause> {
        match self {
            Self::Declined(cause) => Some(*cause),
            Self::Scheduled { .. } => None,
        }
    }

    pub fn retry_class(&self) -> Option<RetryClass> {
        match self {
            Self::Scheduled { class, .. } => Some(*class),
            Self::Declined(_) => None,
        }
    }

    pub fn denial_reason(&self) -> Option<ChargeSafetyDenialReason> {
        match self {
            Self::Declined(RetryDeclineCause::ChargeSafety { reason, .. }) => Some(*reason),
            Self::Scheduled { .. }
            | Self::Declined(
                RetryDeclineCause::NotRetryable
                | RetryDeclineCause::RetryBudgetExhausted
                | RetryDeclineCause::RetryAfterExceedsCap
                | RetryDeclineCause::TimedOut { .. },
            ) => None,
        }
    }

    /// Project host-policy evidence from the decision that owns it.
    pub fn charge_safety(&self) -> Option<ChargeSafetyDecision> {
        match self {
            Self::Scheduled {
                class:
                    RetryClass::ChargeAuthorized {
                        tokens_at_stake,
                        attempt_number,
                    },
                ..
            } => Some(ChargeSafetyDecision::Authorized {
                tokens_at_stake: *tokens_at_stake,
                attempt_number: *attempt_number,
            }),
            Self::Declined(RetryDeclineCause::ChargeSafety {
                tokens_at_stake,
                attempt_number,
                reason,
            }) => Some(ChargeSafetyDecision::Denied {
                tokens_at_stake: *tokens_at_stake,
                attempt_number: *attempt_number,
                reason: *reason,
            }),
            Self::Scheduled {
                class:
                    RetryClass::NoResponse
                    | RetryClass::RejectedHttpResponse
                    | RetryClass::EmptyStreamPartial
                    | RetryClass::ProviderIdempotency
                    | RetryClass::ProviderResume,
                ..
            }
            | Self::Declined(
                RetryDeclineCause::NotRetryable
                | RetryDeclineCause::RetryBudgetExhausted
                | RetryDeclineCause::RetryAfterExceedsCap
                | RetryDeclineCause::TimedOut { .. },
            ) => None,
        }
    }
}
