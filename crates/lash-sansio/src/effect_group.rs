use serde::{Deserialize, Serialize};

/// Wake rule recorded in a durable effect group's identity.
///
/// `Promise.all` and `Promise.allSettled` both use [`All`](Self::All): the
/// caller's early exit on rejection is not a host wake rule. The wire shape
/// has no default, so a missing rule cannot be replayed under a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GroupWakePolicy {
    /// Wake on the first settlement of any kind.
    First,
    /// Wake on the first success, or after every child fails.
    FirstSuccess,
    /// Deliver every settlement in durable rank order.
    All,
}

impl GroupWakePolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::FirstSuccess => "first_success",
            Self::All => "all",
        }
    }
}
