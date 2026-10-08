use super::support::*;
use lash_sansio::llm::capability::{CacheRetention, ReasoningRetentionSelection};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

pub const DEFAULT_THROTTLE_WAIT_BUDGET_MS: u64 = 90_000;

/// One attempt's transport timeouts, read from the attempt's
/// [`ProviderReliability`]. [`ProviderHandle`](super::ProviderHandle)
/// resolves every one against the runtime's provider attempt limits before
/// an attempt is sent ([`ProviderReliability::within`]); a bound still unset
/// (a provider driven outside a handle) is unbounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LlmTimeouts {
    pub request_timeout: Option<Duration>,
    /// The whole-request timeout still wins when it is shorter.
    pub response_start_timeout: Option<Duration>,
    pub chunk_timeout: Option<Duration>,
}

/// The route bound [`RouteBoundAboveBudget`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteBound {
    /// [`ProviderReliability::request_timeout`], against
    /// [`ProviderAttemptLimits::per_request`](lash_sansio::ProviderAttemptLimits::per_request).
    RequestTimeout,
    /// [`ProviderReliability::response_start_timeout`], against
    /// [`ProviderAttemptLimits::response_start`](lash_sansio::ProviderAttemptLimits::response_start).
    ResponseStartTimeout,
    /// [`ProviderReliability::chunk_timeout`], against
    /// [`ProviderAttemptLimits::chunk_idle`](lash_sansio::ProviderAttemptLimits::chunk_idle).
    ChunkTimeout,
    /// [`ProviderRetryPolicy::max_attempts`], against
    /// [`ProviderAttemptLimits::max_attempts`](lash_sansio::ProviderAttemptLimits::max_attempts).
    MaxAttempts,
}

impl std::fmt::Display for RouteBound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::RequestTimeout => "request timeout (ms)",
            Self::ResponseStartTimeout => "response-start timeout (ms)",
            Self::ChunkTimeout => "chunk timeout (ms)",
            Self::MaxAttempts => "max attempts",
        })
    }
}

/// A provider route states a bound above the runtime's provider attempt
/// limit for it: the call is refused, never run under a silently clipped
/// bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the provider route's {bound} of {route} is above the runtime's limit of {budget}")]
pub struct RouteBoundAboveBudget {
    /// Which bound.
    pub bound: RouteBound,
    /// What the route states.
    pub route: u64,
    /// The runtime's limit for it.
    pub budget: u64,
}

/// A provider route's operational options: its reliability policy and its
/// transport byte guards. Publication, cache hints and response-metadata capture
/// come from the recorded model's
/// [`LlmProfileRequestDefaults`](lash_sansio::llm::capability::LlmProfileRequestDefaults).
/// The default output cap comes from its
/// [`OutputTokenLimits`](crate::llm_profile::OutputTokenLimits). Each request
/// carries both as part of its recorded binding.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderOptions {
    #[serde(default)]
    pub reliability: ProviderReliability,
    /// Maximum bytes retained for one SSE event or an unterminated SSE line.
    /// `None` (or `0`) applies the transport default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse_event_bytes: Option<u64>,
    /// `None` (or `0`) applies the transport default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse_total_bytes: Option<u64>,
    /// Maximum raw bytes read before decoding a non-SSE response, including
    /// HTTP errors and auxiliary lookups. `None` uses 16 MiB; zero permits
    /// only an empty body. Content-Length never determines this budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body_bytes: Option<u64>,
}

impl ProviderOptions {
    /// Standard reliability and transport byte guards: 8 MiB per SSE event/line,
    /// 64 MiB total SSE bytes and 16 MiB per buffered HTTP response. The `None`
    /// fields resolve to these guards. These historical values have no workload
    /// measurements; each field is configurable independently.
    pub fn standard() -> Self {
        Self::default()
    }

    /// Effective raw byte budget, clamped to the addressable size on this target.
    pub fn response_body_limit(&self) -> usize {
        usize::try_from(self.response_body_bytes.unwrap_or(16 * 1024 * 1024)).unwrap_or(usize::MAX)
    }

    pub fn is_default(&self) -> bool {
        self.reliability == ProviderReliability::default()
            && self.sse_event_bytes.is_none_or(|bytes| bytes == 0)
            && self.sse_total_bytes.is_none_or(|bytes| bytes == 0)
            && self.response_body_bytes.is_none()
    }

    pub fn llm_timeouts(&self) -> LlmTimeouts {
        self.reliability.llm_timeouts()
    }
}

/// Whether and how a wire carries an output-token cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputCapWire {
    /// The wire has an optional cap field; no cap is sent when none is set.
    Optional,
    /// The wire requires a cap; a call with no effective cap is refused.
    Required,
    /// The wire has no cap field; a set cap is refused.
    Unsupported,
}

/// Where a wire can carry `expose_thinking`'s request for a reasoning summary.
///
/// `expose_thinking` is chiefly local-publication intent: every adapter
/// publishes the reasoning a provider streams when it is set. Only a wire that
/// needs a flag to produce that reasoning gets one; a wire without such a flag
/// sends nothing and the intent is still honored locally. It is never refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingSummaryWire {
    /// No summary field (Chat Completions): nothing is sent.
    NoField,
    /// A summary field that stands on its own.
    Always,
    /// A summary field that exists only inside an active (effort or budget)
    /// thinking configuration.
    WithActiveThinking,
}

/// What one provider wire, as this call uses it, can carry. Each adapter
/// states it; [`resolve_generation_policy`] refuses every host setting the
/// wire cannot carry before any I/O, so an adapter only ever emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationWire {
    /// Lash-authored wire name for refusal messages.
    pub label: &'static str,
    pub output_token_cap: OutputCapWire,
    pub temperature: bool,
    pub seed: bool,
    pub stop_sequences: bool,
    pub parallel_tool_calls: bool,
    pub thinking_summary: ThinkingSummaryWire,
    /// Whether an active effort or budget pins sampling on this wire, the way
    /// Anthropic extended thinking does.
    pub active_thinking_pins_sampling: bool,
}

/// Every host generation setting for one call, resolved once: request options
/// over the model's recorded request defaults, the reasoning selection over
/// the host capability, and every refusal already applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedGenerationPolicy {
    /// The request's cap, else the model's recorded default. `None` sends no
    /// cap.
    pub max_output_tokens: Option<u64>,
    pub output_token_cap_clamped: bool,
    pub temperature: Option<crate::NonNegativeFiniteF64>,
    pub seed: Option<i64>,
    /// Caller-requested literal generation boundaries, copied to the wire
    /// unchanged.
    pub stop_sequences: Vec<String>,
    pub parallel_tool_calls: Option<bool>,
    /// The one reasoning intent the adapter maps onto its wire; `None` sends
    /// no reasoning control.
    pub reasoning: Option<ReasoningIntent>,
    pub cache_retention: CacheRetention,
    /// Local publication of provider reasoning.
    pub expose_thinking: bool,
    /// Whether the adapter must put its reasoning-summary flag on the wire:
    /// `expose_thinking` on a wire that has one for this request.
    pub request_thinking_summary: bool,
}

/// What an adapter put on the wire, reported by the branch that wrote it so
/// the receipt never has to be read back off the body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GenerationEmission {
    pub output_token_cap: bool,
    pub temperature: bool,
    pub seed: bool,
    pub stop_sequences: bool,
    pub parallel_tool_calls: bool,
    pub reasoning: bool,
    /// A native retention field was emitted, or client-side history was projected.
    pub reasoning_retention: bool,
    pub thinking_summary: bool,
    /// The adapter emitted its prompt-cache directive.
    pub cache: bool,
}

fn refused(code: TurnFailureCode, message: String) -> LlmTransportError {
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Unsupported)
        .with_lash_code(code)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
}

fn unsupported(wire: &GenerationWire, setting: &str, reason: &str) -> LlmTransportError {
    refused(
        TurnFailureCode::UnsupportedGenerationOption,
        format!(
            "{} {reason} `{setting}`; clear it for this route instead of expecting lash to drop it.",
            wire.label
        ),
    )
}

/// Resolve every host generation setting for one call against the wire that
/// will carry it. Each setting is sent or refused here, with a typed,
/// non-retryable failure, before the adapter does any I/O; nothing is dropped
/// and nothing is remapped.
///
/// Lash invents no defaults: with no request cap and no recorded
/// `output_tokens.default_cap` no cap is sent, and a wire that requires one
/// refuses the call with `output_token_cap_required`. A pinned model
/// ([`SamplingCapability::Pinned`](lash_sansio::llm::capability::SamplingCapability))
/// or active thinking that pins sampling refuses a set temperature on every
/// adapter.
pub fn resolve_generation_policy(
    request: &LlmRequest,
    provider_kind: &str,
    wire: &GenerationWire,
) -> Result<ResolvedGenerationPolicy, LlmTransportError> {
    let defaults = &request.model.metadata().request_defaults;
    let reasoning = request
        .model
        .metadata()
        .capability
        .reasoning_intent(
            request.model.wire_model(),
            provider_kind,
            &request.model.reasoning,
        )
        .map_err(|error| {
            refused(error.category.failure_code(), error.message)
                .with_kind(ProviderFailureKind::Validation)
        })?;
    let generation = &request.generation;
    let output_tokens = &request.model.metadata().limits.output_tokens;
    let requested_cap = generation.output_token_cap.or(output_tokens.default_cap());
    let effective_cap = match (requested_cap, output_tokens.capacity()) {
        (Some(requested), Some(capacity)) => Some(requested.min(capacity)),
        (requested, None) => requested,
        (None, Some(_)) => None,
    };
    let max_output_tokens = effective_cap.map(|cap| cap.get() as u64);
    let output_token_cap_clamped = requested_cap != effective_cap;
    match (wire.output_token_cap, max_output_tokens) {
        (OutputCapWire::Required, None) => {
            return Err(refused(
                TurnFailureCode::OutputTokenCapRequired,
                format!(
                    "{} requires an output-token cap; set the request's `output_token_cap` or the model's `output_tokens.default_cap`.",
                    wire.label
                ),
            ));
        }
        (OutputCapWire::Unsupported, Some(_)) => {
            return Err(unsupported(wire, "output_token_cap", "has no field for"));
        }
        _ => {}
    }
    if generation.temperature.is_some() {
        if !wire.temperature {
            return Err(unsupported(wire, "temperature", "has no field for"));
        }
        if !request
            .model
            .metadata()
            .capability
            .allows_caller_temperature()
        {
            return Err(unsupported(
                wire,
                "temperature",
                "refuses, because the model's capability pins sampling,",
            ));
        }
        if wire.active_thinking_pins_sampling
            && matches!(
                reasoning,
                Some(ReasoningIntent::Effort(_) | ReasoningIntent::Budget(_))
            )
        {
            return Err(unsupported(
                wire,
                "temperature",
                "refuses, because active thinking pins sampling,",
            ));
        }
    }
    if generation.seed.is_some() && !wire.seed {
        return Err(unsupported(wire, "seed", "has no field for"));
    }
    if !generation.stop_sequences.is_empty() && !wire.stop_sequences {
        return Err(unsupported(wire, "stop_sequences", "has no field for"));
    }
    if generation.parallel_tool_calls.is_some() && !wire.parallel_tool_calls {
        return Err(unsupported(
            wire,
            "parallel_tool_calls",
            "has no field, for this request, for",
        ));
    }
    let request_thinking_summary = defaults.expose_thinking
        && match wire.thinking_summary {
            ThinkingSummaryWire::NoField => false,
            ThinkingSummaryWire::Always => true,
            ThinkingSummaryWire::WithActiveThinking => matches!(
                reasoning,
                Some(ReasoningIntent::Effort(_) | ReasoningIntent::Budget(_))
            ),
        };
    Ok(ResolvedGenerationPolicy {
        max_output_tokens,
        output_token_cap_clamped,
        temperature: generation.temperature.clone(),
        seed: generation.seed,
        stop_sequences: generation.stop_sequences.clone(),
        parallel_tool_calls: generation.parallel_tool_calls,
        reasoning,
        cache_retention: defaults.cache_retention,
        expose_thinking: defaults.expose_thinking,
        request_thinking_summary,
    })
}

impl ResolvedGenerationPolicy {
    /// The per-call receipt: this resolution supplies what the host asked
    /// for, the adapter's `emission` what it put on the wire. `Applied` means
    /// sent, never provider compliance. Resolution reports clamping; the runtime reports protocol stop suppression.
    pub fn receipt(
        &self,
        request: &LlmRequest,
        emission: &GenerationEmission,
    ) -> GenerationReceipt {
        use GenerationOptionOutcome as Outcome;
        let cache_requested = request.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    LlmContentBlock::Text {
                        cache_breakpoint: true,
                        ..
                    }
                )
            })
        });
        GenerationReceipt {
            output_token_cap: if self.output_token_cap_clamped && emission.output_token_cap {
                Outcome::ClampedToCapacity
            } else {
                Outcome::from_emission(self.max_output_tokens.is_some(), emission.output_token_cap)
            },
            temperature: Outcome::from_emission(self.temperature.is_some(), emission.temperature),
            seed: Outcome::from_emission(self.seed.is_some(), emission.seed),
            stop_sequences: Outcome::from_emission(
                !self.stop_sequences.is_empty(),
                emission.stop_sequences,
            ),
            cache: Outcome::from_emission(cache_requested, emission.cache),
            reasoning: Outcome::from_emission(self.reasoning.is_some(), emission.reasoning),
            reasoning_retention: Outcome::from_emission(
                !matches!(
                    request
                        .model
                        .metadata()
                        .capability
                        .reasoning_retention
                        .selection,
                    ReasoningRetentionSelection::ProviderDefault
                ),
                emission.reasoning_retention,
            ),
            parallel_tool_calls: Outcome::from_emission(
                self.parallel_tool_calls.is_some(),
                emission.parallel_tool_calls,
            ),
            // A wire without a summary flag is not asked for one; the
            // host's intent is then carried by local visibility alone.
            thinking_summary: Outcome::from_emission(
                self.request_thinking_summary,
                emission.thinking_summary,
            ),
            // Every adapter gates local reasoning publication on this option.
            thinking_visibility: Outcome::from_emission(self.expose_thinking, true),
            passthrough: Outcome::NotRequested,
        }
    }
}

/// A provider route's reliability. Every unset bound is the runtime's: its
/// [`ExecutionBudgets`](lash_sansio::ExecutionBudgets)' provider attempt
/// limits. A set bound above its limit refuses the call
/// ([`RouteBoundAboveBudget`]); it is never clipped.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ProviderReliability {
    /// Whole-request timeout in milliseconds. `None` (or `0`) is the
    /// runtime's per-request limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout: Option<u64>,
    /// Streaming response-start timeout in milliseconds. `None` (or `0`) is
    /// the runtime's response-start limit. Once the response starts, only
    /// the whole-request and inter-chunk timeouts apply. "Start" is the
    /// response headers on HTTP/SSE providers and the first response frame
    /// on the Codex WebSocket path: an HTTP provider that returns headers
    /// promptly and then stalls before the first body byte is bounded by
    /// the inter-chunk timeout, not this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_start_timeout: Option<u64>,
    /// Inter-chunk stream timeout in milliseconds. `None` (or `0`) is the
    /// runtime's chunk-idle limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_timeout: Option<u64>,
    #[serde(default)]
    pub retry: ProviderRetryPolicy,
    #[serde(default)]
    pub rate_limits: ProviderRateLimitPolicy,
}

fn whole_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

impl ProviderReliability {
    /// Standard retry policy, no rate gates, and timeouts inherited from the
    /// host's runtime attempt limits. See [`ProviderRetryPolicy::standard`] for values
    /// and their lack of workload measurements.
    pub fn standard() -> Self {
        Self::default()
    }

    pub fn disabled() -> Self {
        Self {
            retry: ProviderRetryPolicy::disabled(),
            ..Self::default()
        }
    }

    /// This route under the runtime's provider attempt `limits`: every unset
    /// bound and attempt count is the limit's own, and every set one is
    /// kept.
    ///
    /// # Errors
    ///
    /// [`RouteBoundAboveBudget`] for the first set bound above its limit.
    pub fn within(
        &self,
        limits: &lash_sansio::ProviderAttemptLimits,
    ) -> Result<Self, RouteBoundAboveBudget> {
        let bound = |bound: RouteBound, route: Option<u64>, limit: u64| match route
            .filter(|value| *value > 0)
        {
            None => Ok(limit),
            Some(route) if route <= limit => Ok(route),
            Some(route) => Err(RouteBoundAboveBudget {
                bound,
                route,
                budget: limit,
            }),
        };
        let mut resolved = self.clone();
        resolved.request_timeout = Some(bound(
            RouteBound::RequestTimeout,
            self.request_timeout,
            whole_millis(limits.per_request()),
        )?);
        resolved.response_start_timeout = Some(bound(
            RouteBound::ResponseStartTimeout,
            self.response_start_timeout,
            whole_millis(limits.response_start()),
        )?);
        resolved.chunk_timeout = Some(bound(
            RouteBound::ChunkTimeout,
            self.chunk_timeout,
            whole_millis(limits.chunk_idle()),
        )?);
        resolved.retry.max_attempts = Some(if self.retry.enabled {
            let attempts = bound(
                RouteBound::MaxAttempts,
                self.retry.max_attempts.map(u64::from),
                u64::from(limits.max_attempts()),
            )?;
            u32::try_from(attempts).unwrap_or(u32::MAX)
        } else {
            1
        });
        Ok(resolved)
    }

    /// This route's timeouts for one attempt, each clipped to `window`: what
    /// remains of the model call's total.
    #[must_use]
    pub fn for_window(&self, window: Duration) -> Self {
        let window = whole_millis(window);
        let clip = |bound: Option<u64>| {
            Some(
                bound
                    .filter(|value| *value > 0)
                    .map_or(window, |value| value.min(window)),
            )
        };
        Self {
            request_timeout: clip(self.request_timeout),
            response_start_timeout: clip(self.response_start_timeout),
            chunk_timeout: clip(self.chunk_timeout),
            ..self.clone()
        }
    }

    /// The transport timeouts this reliability states.
    pub fn llm_timeouts(&self) -> LlmTimeouts {
        let bound =
            |value: Option<u64>| value.filter(|value| *value > 0).map(Duration::from_millis);
        LlmTimeouts {
            request_timeout: bound(self.request_timeout),
            response_start_timeout: bound(self.response_start_timeout),
            chunk_timeout: bound(self.chunk_timeout),
        }
    }

    pub fn request_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.request_timeout = timeout_ms;
        self
    }

    /// `None` (or `0`) is the runtime's response-start limit; this does not
    /// change the inter-chunk timeout after the response starts.
    pub fn response_start_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.response_start_timeout = timeout_ms;
        self
    }

    pub fn stream_chunk_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.chunk_timeout = timeout_ms;
        self
    }

    /// `None` is the runtime's attempt limit.
    pub fn max_attempts(mut self, attempts: Option<u32>) -> Self {
        self.retry.max_attempts = attempts.map(|attempts| attempts.max(1));
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

    pub fn requests_per_window(mut self, rate: Option<ProviderRateWindow>) -> Self {
        self.rate_limits.requests_per_window = rate;
        self
    }

    pub fn tokens_per_window(mut self, rate: Option<ProviderRateWindow>) -> Self {
        self.rate_limits.tokens_per_window = rate;
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderRetryPolicy {
    pub enabled: bool,
    /// Attempts one model call may make, the first included. `None` is the
    /// runtime's attempt limit; a count above it refuses the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    /// Upper bound for uniform random jitter added to ordinary retry backoff
    /// on each attempt. The default is 500 ms; set to `0` to disable jitter.
    pub jitter_ms: u64,
    /// Maximum provider-stated `Retry-After` honored by the host. A longer
    /// delay fails the attempt immediately instead of being truncated or
    /// slept. `None` deliberately accepts an unbounded provider duration;
    /// selecting it means the host accepts that a hostile header can stall a
    /// completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_cap_ms: Option<u64>,
    /// Cumulative time [`ProviderHandle::complete`](super::ProviderHandle::complete)
    /// may spend honoring provider throttle waits — a retryable [`ProviderFailureKind::Quota`]
    /// failure carrying `Retry-After` — without consuming retry attempts.
    /// Waits at least `courtesy_min_wait_ms` qualify, and each deferred wait
    /// charges what it actually waits. At most `courtesy_call_limit` calls are deferred;
    /// total provider calls are therefore bounded by that limit plus
    /// `max_attempts`, independently of `Retry-After`. Once either bound is
    /// spent, throttled failures consume attempts like any other retryable
    /// failure. `0` disables the deference entirely.
    #[serde(
        default = "default_throttle_wait_budget_ms",
        skip_serializing_if = "is_default_throttle_wait_budget_ms"
    )]
    pub throttle_wait_budget_ms: u64,
    /// Additional calls allowed outside the counted attempt ladder. Zero disables courtesy calls.
    #[serde(default = "default_courtesy_call_limit")]
    pub courtesy_call_limit: usize,
    /// Minimum provider wait eligible for courtesy handling and direct Retry-After backoff.
    /// Zero accepts even a zero wait; the courtesy call limit still bounds calls.
    #[serde(default = "default_courtesy_min_wait_ms")]
    pub courtesy_min_wait_ms: u64,
}

fn default_courtesy_call_limit() -> usize {
    8
}
fn default_courtesy_min_wait_ms() -> u64 {
    1_000
}

fn default_throttle_wait_budget_ms() -> u64 {
    DEFAULT_THROTTLE_WAIT_BUDGET_MS
}

fn is_default_throttle_wait_budget_ms(budget_ms: &u64) -> bool {
    *budget_ms == DEFAULT_THROTTLE_WAIT_BUDGET_MS
}

impl Default for ProviderRetryPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

impl ProviderRetryPolicy {
    /// Standard retry preset: enabled; attempts inherit the runtime; 2 s base,
    /// 10 s maximum backoff plus up to 500 ms jitter; 60 s Retry-After cap;
    /// 90 s courtesy wait budget, eight courtesy calls and a 1 s minimum wait.
    /// These are historical operational choices without workload measurements.
    pub fn standard() -> Self {
        Self {
            enabled: true,
            max_attempts: None,
            base_delay_ms: 2_000,
            max_delay_ms: 10_000,
            jitter_ms: 500,
            retry_after_cap_ms: Some(60_000),
            throttle_wait_budget_ms: DEFAULT_THROTTLE_WAIT_BUDGET_MS,
            courtesy_call_limit: default_courtesy_call_limit(),
            courtesy_min_wait_ms: default_courtesy_min_wait_ms(),
        }
    }
}

impl ProviderRetryPolicy {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            max_attempts: Some(1),
            base_delay_ms: 0,
            max_delay_ms: 0,
            jitter_ms: 0,
            retry_after_cap_ms: None,
            throttle_wait_budget_ms: 0,
            courtesy_call_limit: 0,
            courtesy_min_wait_ms: 0,
        }
    }

    /// Return a provider-stated delay only when it is within the host cap.
    /// `None` means the server asked the host to wait beyond that bound and
    /// the attempt must fail immediately.
    pub(crate) fn retry_after_within_cap(&self, retry_after: Duration) -> Option<Duration> {
        self.retry_after_cap_ms
            .map(Duration::from_millis)
            .is_none_or(|cap| retry_after <= cap)
            .then_some(retry_after)
    }

    pub(crate) fn delay_for_attempt(
        &self,
        retry_index: u32,
        retry_after: Option<Duration>,
    ) -> Option<Duration> {
        if let Some(retry_after) = retry_after {
            let retry_after = self.retry_after_within_cap(retry_after)?;
            if retry_after >= Duration::from_millis(self.courtesy_min_wait_ms) {
                return Some(retry_after);
            }
        }
        let multiplier = 1u64.checked_shl(retry_index).unwrap_or(u64::MAX);
        let delay_ms = self
            .base_delay_ms
            .saturating_mul(multiplier)
            .min(self.max_delay_ms);
        Some(Duration::from_millis(
            delay_ms.saturating_add(self.sample_jitter_ms(retry_index)),
        ))
    }

    fn sample_jitter_ms(&self, retry_index: u32) -> u64 {
        if self.jitter_ms == 0 {
            return 0;
        }

        let entropy = RandomState::new();
        let mut draw = 0u64;
        loop {
            let mut hasher = entropy.build_hasher();
            hasher.write_u64(self.base_delay_ms);
            hasher.write_u64(self.max_delay_ms);
            hasher.write_u64(self.jitter_ms);
            hasher.write_u32(retry_index);
            hasher.write_u64(draw);
            let sample = hasher.finish();

            if self.jitter_ms == u64::MAX {
                return sample;
            }
            let width = self.jitter_ms + 1;
            let unbiased_zone = u64::MAX - u64::MAX % width;
            if sample < unbiased_zone {
                return sample % width;
            }
            draw = draw.wrapping_add(1);
        }
    }
}

/// A configured rate always states both its count and its positive window.
/// There is no implicit one-minute window and no zero-as-disabled encoding.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderRateWindow {
    pub count: std::num::NonZeroU32,
    pub window_ms: std::num::NonZeroU64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderRateLimitPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    /// None disables request-rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_window: Option<ProviderRateWindow>,
    /// None disables token-rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_window: Option<ProviderRateWindow>,
}

impl ProviderRateLimitPolicy {
    /// No concurrency, request or token gate. This neutral preset selects no rate.
    pub fn standard() -> Self {
        Self::default()
    }
}

/// Per-call retry accounting; courtesy calls never consume the counted ladder.
#[derive(Debug, Default)]
pub(super) struct RetryBudget {
    pub(super) attempt: u32,
    pub(super) throttle_waited: Duration,
    courtesy_calls: usize,
    pub(super) unsafe_retries: u8,
}

impl RetryBudget {
    pub(super) fn throttle_wait(
        &self,
        policy: &ProviderRetryPolicy,
        verdict: TransportRetryVerdict,
    ) -> Option<Duration> {
        let TransportRetryVerdict::RetryableThrottle {
            retry_after: Some(wait),
        } = verdict
        else {
            return None;
        };
        let wait = policy.retry_after_within_cap(wait)?;
        (wait >= Duration::from_millis(policy.courtesy_min_wait_ms)
            && self.courtesy_calls < policy.courtesy_call_limit
            && self.throttle_waited.saturating_add(wait)
                <= Duration::from_millis(policy.throttle_wait_budget_ms))
        .then_some(wait)
    }

    pub(super) fn charge_throttle(&mut self, wait: Duration, unsafe_retry: bool) {
        self.throttle_waited += wait;
        self.courtesy_calls += 1;
        self.charge_generation(unsafe_retry);
    }

    pub(super) fn consume(&mut self, unsafe_retry: bool) {
        self.attempt += 1;
        self.charge_generation(unsafe_retry);
    }

    fn charge_generation(&mut self, unsafe_retry: bool) {
        if unsafe_retry {
            self.unsafe_retries = self.unsafe_retries.saturating_add(1);
        }
    }
}
