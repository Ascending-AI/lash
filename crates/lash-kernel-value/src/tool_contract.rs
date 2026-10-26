/// The contract for executing one logical call, pinned before its first attempt.
///
/// Recovery never re-executes an effect whose outcome committed. These rules
/// require acknowledged database commits to survive the failure; exactly-once
/// external behaviour requires host deduplication on `call_id`. See
/// [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionPolicy {
    /// One application attempt, even after a reported pre-effect failure.
    /// A crash after its start commits but before its outcome commits settles
    /// `Interrupted` without re-entering the body. Its external action may have
    /// happened; this policy does not promise successful completion.
    #[default]
    Once,
    /// Repeated execution is part of the tool's declared contract.
    /// An interrupted attempt may re-enter its same ordinal; reported failures
    /// retry within the recorded bounds and current declaration. `max_attempts`
    /// counts admitted ordinals, not body entries across repeated crashes.
    Repeatable { retry: BoundedRetry },
    /// A read: the answer is not recorded; after a crash before the next
    /// save it is fetched again and may differ.
    ///
    /// A code cell runs the call inside the statement that performs it,
    /// through the provider's registered reader, and keeps the answer only
    /// as part of the cell's saved state. No admission, attempt or outcome
    /// is recorded for it, and its provider stays owed until the cell ends.
    /// A call that is recorded anyway (a model round's, or one a cell
    /// cannot plan) runs again from its start when its attempt was
    /// interrupted, and is never retried after a reported failure. A read
    /// changes nothing outside: it defers, isolates, declares intents and
    /// ends turns never.
    Read,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct BoundedRetry {
    /// Counts admitted application ordinals, including the first attempt.
    pub max_attempts: std::num::NonZeroU32,
    pub backoff: Backoff,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Backoff {
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl ExecutionPolicy {
    pub fn repeatable(
        max_attempts: std::num::NonZeroU32,
        base_delay_ms: u64,
        max_delay_ms: u64,
    ) -> Self {
        Self::Repeatable {
            retry: BoundedRetry {
                max_attempts,
                backoff: Backoff {
                    base_delay_ms,
                    max_delay_ms,
                },
            },
        }
    }

    /// The bounded retry a `Repeatable` call is admitted with; `None` for a
    /// policy that never retries a reported failure.
    pub fn retry(self) -> Option<BoundedRetry> {
        match self {
            Self::Repeatable { retry } => Some(retry),
            Self::Once | Self::Read => None,
        }
    }

    /// A current declaration may veto a repeat; it cannot upgrade admission.
    pub fn permits_repeat(self, current: Self, failed_ordinal: u32) -> bool {
        matches!((self, current), (Self::Repeatable { retry }, Self::Repeatable { .. }) if failed_ordinal < retry.max_attempts.get())
    }

    /// Whether an admitted attempt that started and never settled runs
    /// again at its ordinal, rather than settling `Interrupted`: a
    /// `Repeatable` call, whose contract permits repeats, and a recorded
    /// `Read`, which changes nothing outside.
    pub fn reruns_interrupted(self) -> bool {
        matches!(self, Self::Repeatable { .. } | Self::Read)
    }
}

impl BoundedRetry {
    /// The delay before retry `retry_index` (0 for the first retry): the
    /// failure's suggested delay, or the doubling backoff from its base,
    /// capped at its maximum.
    pub fn delay_ms(self, retry_index: u32, suggested_delay_ms: Option<u64>) -> u64 {
        let multiplier = 1_u64.checked_shl(retry_index).unwrap_or(u64::MAX);
        suggested_delay_ms
            .unwrap_or_else(|| self.backoff.base_delay_ms.saturating_mul(multiplier))
            .min(self.backoff.max_delay_ms)
    }
}
