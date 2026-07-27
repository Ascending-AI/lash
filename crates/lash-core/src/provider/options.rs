use super::support::*;

pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_CHUNK_TIMEOUT_MS: u64 = 120_000;
pub const DEFAULT_THROTTLE_WAIT_BUDGET_MS: u64 = 90_000;

/// Minimum amount a single deferred throttle wait charges against
/// [`ProviderRetryPolicy::throttle_wait_budget_ms`]. Charging at least this
/// much per deference bounds the courtesy in count as well as time: a zero or
/// near-zero `Retry-After` (or a zero [`retry_after_cap_ms`]) cannot spin the
/// attempt-free retry loop more than `budget / charge` times.
///
/// [`retry_after_cap_ms`]: ProviderRetryPolicy::retry_after_cap_ms
pub(crate) const MIN_THROTTLE_BUDGET_CHARGE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LlmTimeouts {
    pub request_timeout: Option<Duration>,
    pub chunk_timeout: Duration,
}

impl Default for LlmTimeouts {
    fn default() -> Self {
        Self {
            request_timeout: Some(Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS)),
            chunk_timeout: Duration::from_millis(DEFAULT_CHUNK_TIMEOUT_MS),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestTimeout {
    Disabled,
    Millis(u64),
}

impl Serialize for RequestTimeout {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Disabled => serializer.serialize_bool(false),
            Self::Millis(value) => serializer.serialize_u64(*value),
        }
    }
}

impl<'de> Deserialize<'de> for RequestTimeout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RequestTimeoutVisitor;

        impl Visitor<'_> for RequestTimeoutVisitor {
            type Value = RequestTimeout;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a positive timeout in milliseconds or false")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value {
                    return Err(E::custom("timeout must be a positive integer or false"));
                }
                Ok(RequestTimeout::Disabled)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == 0 {
                    return Err(E::custom("timeout must be greater than 0"));
                }
                Ok(RequestTimeout::Millis(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value <= 0 {
                    return Err(E::custom("timeout must be greater than 0"));
                }
                Ok(RequestTimeout::Millis(value as u64))
            }
        }

        deserializer.deserialize_any(RequestTimeoutVisitor)
    }
}

/// Prompt-cache lifetime hint. Providers translate this into their own
/// wire dialect (Anthropic and OpenRouter Claude/Gemini `cache_control`,
/// OpenAI Responses and Codex `prompt_cache_key`, and OpenAI
/// `prompt_cache_retention`). Providers without a cache-control concept,
/// such as direct Google, read the value but emit nothing for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheRetention {
    /// Do not emit any prompt-cache hints.
    None,
    /// Default Anthropic ephemeral window (5 minutes).
    #[default]
    Short,
    /// Extend to a 1-hour TTL where the API supports it.
    Long,
}

impl CacheRetention {
    pub fn is_default(&self) -> bool {
        matches!(self, CacheRetention::Short)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderOptions {
    #[serde(default)]
    pub reliability: ProviderReliability,
    /// Surface provider reasoning/thinking output in responses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expose_thinking: bool,
    /// Per-request output-token cap. `None` lets each provider apply its
    /// own default. Providers translate to their wire-specific field
    /// (`max_tokens`, `max_output_tokens`, `maxOutputTokens`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Prompt-cache lifetime hint; see [`CacheRetention`].
    #[serde(default, skip_serializing_if = "CacheRetention::is_default")]
    pub cache_retention: CacheRetention,
}

impl ProviderOptions {
    pub fn is_default(&self) -> bool {
        self.reliability == ProviderReliability::default()
            && !self.expose_thinking
            && self.max_output_tokens.is_none()
            && self.cache_retention.is_default()
    }

    pub fn llm_timeouts(&self) -> LlmTimeouts {
        self.reliability.llm_timeouts()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedGenerationPolicy<TThinking> {
    pub max_output_tokens: u64,
    /// Requested sampling temperature, or `None` when the caller expressed no
    /// preference. Adapters with an endpoint default of their own layer it
    /// beneath this value.
    pub temperature: Option<crate::NonNegativeFiniteF64>,
    /// Requested sampling seed, or `None`. Only wires that accept a seed emit
    /// it; the rest omit it.
    pub seed: Option<i64>,
    pub cache_retention: CacheRetention,
    pub expose_thinking: bool,
    pub thinking: TThinking,
}

pub fn resolve_generation_policy<TThinking>(
    generation: &crate::GenerationOptions,
    options: &ProviderOptions,
    provider_default_max_output_tokens: u64,
    thinking: TThinking,
) -> ResolvedGenerationPolicy<TThinking> {
    let max_output_tokens = generation
        .output_token_cap_u64()
        .or(options.max_output_tokens)
        .unwrap_or(provider_default_max_output_tokens);
    ResolvedGenerationPolicy {
        max_output_tokens,
        temperature: generation.temperature.clone(),
        seed: generation.seed,
        cache_retention: options.cache_retention,
        expose_thinking: options.expose_thinking,
        thinking,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ProviderReliability {
    /// Whole-request timeout. `None` applies [`DEFAULT_REQUEST_TIMEOUT_MS`];
    /// use [`RequestTimeout::Disabled`] to wait indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout: Option<RequestTimeout>,
    /// Inter-chunk stream timeout in milliseconds. `None` (or `0`) applies
    /// [`DEFAULT_CHUNK_TIMEOUT_MS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_timeout: Option<u64>,
    #[serde(default)]
    pub retry: ProviderRetryPolicy,
    #[serde(default)]
    pub rate_limits: ProviderRateLimitPolicy,
}

impl ProviderReliability {
    pub fn codex() -> Self {
        Self {
            retry: ProviderRetryPolicy {
                max_attempts: 4,
                base_delay_ms: 1_000,
                max_delay_ms: 4_000,
                jitter_ms: 0,
                retry_after_cap_ms: Some(60_000),
                throttle_wait_budget_ms: DEFAULT_THROTTLE_WAIT_BUDGET_MS,
                enabled: true,
            },
            ..Self::default()
        }
    }

    pub fn disabled() -> Self {
        Self {
            retry: ProviderRetryPolicy::disabled(),
            ..Self::default()
        }
    }

    pub fn llm_timeouts(&self) -> LlmTimeouts {
        let request_timeout = match self.request_timeout {
            Some(RequestTimeout::Disabled) => None,
            Some(RequestTimeout::Millis(ms)) => Some(Duration::from_millis(ms)),
            None => Some(Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS)),
        };
        let chunk_timeout_ms = self
            .chunk_timeout
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_CHUNK_TIMEOUT_MS);
        LlmTimeouts {
            request_timeout,
            chunk_timeout: Duration::from_millis(chunk_timeout_ms),
        }
    }

    pub fn request_timeout(mut self, timeout: Option<RequestTimeout>) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn stream_chunk_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.chunk_timeout = timeout_ms;
        self
    }

    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.retry.max_attempts = attempts.max(1);
        self
    }

    pub fn base_delay_ms(mut self, delay_ms: u64) -> Self {
        self.retry.base_delay_ms = delay_ms;
        self
    }

    pub fn max_delay_ms(mut self, delay_ms: u64) -> Self {
        self.retry.max_delay_ms = delay_ms;
        self
    }

    pub fn retry_after_cap_ms(mut self, cap_ms: Option<u64>) -> Self {
        self.retry.retry_after_cap_ms = cap_ms;
        self
    }

    pub fn throttle_wait_budget_ms(mut self, budget_ms: u64) -> Self {
        self.retry.throttle_wait_budget_ms = budget_ms;
        self
    }

    pub fn max_concurrency(mut self, value: Option<usize>) -> Self {
        self.rate_limits.max_concurrency = value;
        self
    }

    pub fn requests_per_window(mut self, requests: Option<u32>, window_ms: Option<u64>) -> Self {
        self.rate_limits.requests_per_window = requests;
        self.rate_limits.request_window_ms = window_ms;
        self
    }

    pub fn tokens_per_window(mut self, tokens: Option<u32>, window_ms: Option<u64>) -> Self {
        self.rate_limits.tokens_per_window = tokens;
        self.rate_limits.token_window_ms = window_ms;
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderRetryPolicy {
    pub enabled: bool,
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    pub jitter_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_cap_ms: Option<u64>,
    /// Cumulative time [`ProviderHandle::complete`] may spend honoring
    /// provider throttle waits — a retryable [`ProviderFailureKind::Quota`]
    /// failure carrying `Retry-After` — without consuming retry attempts.
    /// Each deferred wait charges what it actually waits, with a one-second
    /// floor per deference, so the courtesy is bounded in count as well as
    /// time. Once the budget is spent, throttled failures consume attempts
    /// like any other retryable failure. `0` disables the deference entirely.
    #[serde(
        default = "default_throttle_wait_budget_ms",
        skip_serializing_if = "is_default_throttle_wait_budget_ms"
    )]
    pub throttle_wait_budget_ms: u64,
}

fn default_throttle_wait_budget_ms() -> u64 {
    DEFAULT_THROTTLE_WAIT_BUDGET_MS
}

fn is_default_throttle_wait_budget_ms(budget_ms: &u64) -> bool {
    *budget_ms == DEFAULT_THROTTLE_WAIT_BUDGET_MS
}

impl Default for ProviderRetryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 4,
            base_delay_ms: 2_000,
            max_delay_ms: 10_000,
            jitter_ms: 0,
            retry_after_cap_ms: Some(60_000),
            throttle_wait_budget_ms: DEFAULT_THROTTLE_WAIT_BUDGET_MS,
        }
    }
}

impl ProviderRetryPolicy {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            max_attempts: 1,
            base_delay_ms: 0,
            max_delay_ms: 0,
            jitter_ms: 0,
            retry_after_cap_ms: None,
            throttle_wait_budget_ms: 0,
        }
    }

    pub(crate) fn attempts(&self) -> u32 {
        if self.enabled {
            self.max_attempts.max(1)
        } else {
            1
        }
    }

    /// A provider-stated `Retry-After`, bounded by
    /// [`retry_after_cap_ms`](Self::retry_after_cap_ms) when one is set.
    pub(crate) fn cap_retry_after(&self, retry_after: Duration) -> Duration {
        self.retry_after_cap_ms
            .map(Duration::from_millis)
            .map(|cap| retry_after.min(cap))
            .unwrap_or(retry_after)
    }

    pub(crate) fn delay_for_attempt(
        &self,
        retry_index: u32,
        retry_after: Option<Duration>,
    ) -> Duration {
        if let Some(retry_after) = retry_after {
            return self.cap_retry_after(retry_after);
        }
        let multiplier = 1u64.checked_shl(retry_index).unwrap_or(u64::MAX);
        let delay_ms = self
            .base_delay_ms
            .saturating_mul(multiplier)
            .min(self.max_delay_ms);
        Duration::from_millis(delay_ms.saturating_add(self.jitter_ms))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ProviderRateLimitPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_window_ms: Option<u64>,
}
