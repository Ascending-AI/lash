//! Execution budgets, execution limits and the inline ceiling (spec v3
//! Parts C and E; ADR 0132 §7).
//!
//! One validated [`ExecutionBudgets`] is the only source of every execution
//! bound: the tool default and ceiling, the model call's hard total, the
//! control-phase bound, the stop grace, the wait default and ceiling, and the
//! provider attempt limits. Nothing else carries a timeout constant of its own.
//!
//! An [`ExecutionLimit`] is one executable stretch's bound, minted from the
//! budgets at an instant of lash's injected clock. Nested stretches take
//! `min(own, enclosing remaining)` and a limit is never refreshed.

use std::num::NonZeroU32;
use std::time::Duration;

use crate::{ToolDeclaration, ToolIntentKind, ToolManifest};

/// The most provider attempts one model call may make. A larger count is
/// an unbounded retry in all but name.
pub const MAX_PROVIDER_ATTEMPTS: u32 = 16;

/// The longest bound any budget may hold: 30 days. Every instant a budget is
/// added to stays representable in epoch milliseconds.
pub const MAX_EXECUTION_BUDGET: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Why a budget set, or a provider attempt-limit set, is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExecutionBudgetsError {
    /// A bound is zero, below one millisecond, or above
    /// [`MAX_EXECUTION_BUDGET`].
    #[error("execution budget `{field}` must be between 1 ms and {max:?}, not {value:?}")]
    OutOfRange {
        field: &'static str,
        value: Duration,
        max: Duration,
    },
    /// The sum of a stretch and its grace does not fit in a budget.
    #[error("execution budgets `{left}` + `{right}` overflow {max:?}")]
    SumOverflows {
        left: &'static str,
        right: &'static str,
        max: Duration,
    },
    /// A default above the ceiling it defaults under.
    #[error(
        "execution budget `{default}` ({default_value:?}) exceeds `{ceiling}` ({ceiling_value:?})"
    )]
    DefaultExceedsCeiling {
        default: &'static str,
        default_value: Duration,
        ceiling: &'static str,
        ceiling_value: Duration,
    },
    /// A provider retry count of zero or above [`MAX_PROVIDER_ATTEMPTS`].
    #[error("provider attempts must be between 1 and {max}, not {value}")]
    UnboundedRetry { value: u32, max: u32 },
}

fn bounded(field: &'static str, value: Duration) -> Result<Duration, ExecutionBudgetsError> {
    if value < Duration::from_millis(1) || value > MAX_EXECUTION_BUDGET {
        return Err(ExecutionBudgetsError::OutOfRange {
            field,
            value,
            max: MAX_EXECUTION_BUDGET,
        });
    }
    Ok(value)
}

fn bounded_sum(
    (left, left_value): (&'static str, Duration),
    (right, right_value): (&'static str, Duration),
) -> Result<(), ExecutionBudgetsError> {
    match left_value.checked_add(right_value) {
        Some(sum) if sum <= MAX_EXECUTION_BUDGET => Ok(()),
        _ => Err(ExecutionBudgetsError::SumOverflows {
            left,
            right,
            max: MAX_EXECUTION_BUDGET,
        }),
    }
}

fn not_above(
    (default, default_value): (&'static str, Duration),
    (ceiling, ceiling_value): (&'static str, Duration),
) -> Result<(), ExecutionBudgetsError> {
    if default_value > ceiling_value {
        return Err(ExecutionBudgetsError::DefaultExceedsCeiling {
            default,
            default_value,
            ceiling,
            ceiling_value,
        });
    }
    Ok(())
}

/// The bounds of one provider attempt and the number of attempts a model
/// call may make. Every bound is clipped to the call's remaining
/// [`ExecutionBudgets::model_total`] before an attempt starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderAttemptLimits {
    per_request: Duration,
    response_start: Duration,
    chunk_idle: Duration,
    max_attempts: NonZeroU32,
}

impl ProviderAttemptLimits {
    /// Validated provider attempt limits.
    ///
    /// # Errors
    ///
    /// A bound out of range, a response-start or chunk-idle bound above the
    /// whole request, or an attempt count outside `1..=MAX_PROVIDER_ATTEMPTS`.
    pub fn new(
        per_request: Duration,
        response_start: Duration,
        chunk_idle: Duration,
        max_attempts: u32,
    ) -> Result<Self, ExecutionBudgetsError> {
        let per_request = bounded("provider.per_request", per_request)?;
        let response_start = bounded("provider.response_start", response_start)?;
        let chunk_idle = bounded("provider.chunk_idle", chunk_idle)?;
        not_above(
            ("provider.response_start", response_start),
            ("provider.per_request", per_request),
        )?;
        not_above(
            ("provider.chunk_idle", chunk_idle),
            ("provider.per_request", per_request),
        )?;
        let max_attempts = NonZeroU32::new(max_attempts)
            .filter(|attempts| attempts.get() <= MAX_PROVIDER_ATTEMPTS)
            .ok_or(ExecutionBudgetsError::UnboundedRetry {
                value: max_attempts,
                max: MAX_PROVIDER_ATTEMPTS,
            })?;
        Ok(Self {
            per_request,
            response_start,
            chunk_idle,
            max_attempts,
        })
    }

    /// The whole-request bound of one attempt.
    #[must_use]
    pub fn per_request(&self) -> Duration {
        self.per_request
    }

    /// The bound on a streaming response's start.
    #[must_use]
    pub fn response_start(&self) -> Duration {
        self.response_start
    }

    /// The bound on silence between stream chunks.
    #[must_use]
    pub fn chunk_idle(&self) -> Duration {
        self.chunk_idle
    }

    /// Attempts one model call may make, the first included.
    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts.get()
    }
}

impl Default for ProviderAttemptLimits {
    /// 5 min per request, 2 min to response start, 2 min of chunk silence,
    /// 4 attempts.
    fn default() -> Self {
        Self {
            per_request: Duration::from_secs(5 * 60),
            response_start: Duration::from_secs(2 * 60),
            chunk_idle: Duration::from_secs(2 * 60),
            max_attempts: NonZeroU32::MIN.saturating_add(3),
        }
    }
}

/// The values an [`ExecutionBudgets`] is built from. [`Default`] holds the
/// shipped defaults; [`ExecutionBudgets::new`] validates a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionBudgetsConfig {
    /// One inline tool execution with no declared duration: 2 min.
    pub tool_default: Duration,
    /// The longest inline tool execution a tool may declare: 5 min.
    pub tool_ceiling: Duration,
    /// The hard cap on one model call, over throttle, backoff and every
    /// provider attempt: 10 min.
    pub model_total: Duration,
    /// One admission or checkpoint phase, all of its checks together: 60 s.
    pub control_phase: Duration,
    /// Spent once, after a stretch ends at its limit or on cancellation, to
    /// collect evidence: 2 s.
    pub stop_grace: Duration,
    /// One deferred or external wait with no declared deadline: 1 h.
    pub wait_default: Duration,
    /// The longest deferred or external wait: 24 h.
    pub wait_ceiling: Duration,
    pub provider: ProviderAttemptLimits,
}

impl Default for ExecutionBudgetsConfig {
    fn default() -> Self {
        Self {
            tool_default: Duration::from_secs(2 * 60),
            tool_ceiling: Duration::from_secs(5 * 60),
            model_total: Duration::from_secs(10 * 60),
            control_phase: Duration::from_secs(60),
            stop_grace: Duration::from_secs(2),
            wait_default: Duration::from_secs(60 * 60),
            wait_ceiling: Duration::from_secs(24 * 60 * 60),
            provider: ProviderAttemptLimits::default(),
        }
    }
}

/// The one validated source of every execution bound (spec v3 Part C).
/// Shared: every holder of a runtime's budgets reads the same value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionBudgets(std::sync::Arc<ExecutionBudgetsConfig>);

impl ExecutionBudgets {
    /// Validate `config`.
    ///
    /// # Errors
    ///
    /// A non-positive or unrepresentable bound, a stretch whose grace
    /// overflows, a default above its ceiling, a provider sublimit above the
    /// model total, or an unbounded provider retry.
    pub fn new(config: ExecutionBudgetsConfig) -> Result<Self, ExecutionBudgetsError> {
        let ExecutionBudgetsConfig {
            tool_default,
            tool_ceiling,
            model_total,
            control_phase,
            stop_grace,
            wait_default,
            wait_ceiling,
            provider,
        } = config;
        bounded("tool_default", tool_default)?;
        bounded("tool_ceiling", tool_ceiling)?;
        bounded("model_total", model_total)?;
        bounded("control_phase", control_phase)?;
        bounded("stop_grace", stop_grace)?;
        bounded("wait_default", wait_default)?;
        bounded("wait_ceiling", wait_ceiling)?;
        // Construction through `ProviderAttemptLimits::new` is the only
        // way to a value, so it is already in range.
        not_above(
            ("tool_default", tool_default),
            ("tool_ceiling", tool_ceiling),
        )?;
        not_above(
            ("wait_default", wait_default),
            ("wait_ceiling", wait_ceiling),
        )?;
        not_above(
            ("provider.per_request", provider.per_request),
            ("model_total", model_total),
        )?;
        bounded_sum(("tool_ceiling", tool_ceiling), ("stop_grace", stop_grace))?;
        bounded_sum(("model_total", model_total), ("stop_grace", stop_grace))?;
        bounded_sum(("control_phase", control_phase), ("stop_grace", stop_grace))?;
        bounded_sum(
            ("wait_ceiling", wait_ceiling),
            ("tool_ceiling", tool_ceiling),
        )?;
        Ok(Self(std::sync::Arc::new(config)))
    }

    /// The values this budget set was built from.
    #[must_use]
    pub fn config(&self) -> ExecutionBudgetsConfig {
        *self.0
    }

    #[must_use]
    pub fn tool_default(&self) -> Duration {
        self.0.tool_default
    }

    #[must_use]
    pub fn tool_ceiling(&self) -> Duration {
        self.0.tool_ceiling
    }

    #[must_use]
    pub fn model_total(&self) -> Duration {
        self.0.model_total
    }

    #[must_use]
    pub fn control_phase(&self) -> Duration {
        self.0.control_phase
    }

    #[must_use]
    pub fn stop_grace(&self) -> Duration {
        self.0.stop_grace
    }

    #[must_use]
    pub fn wait_default(&self) -> Duration {
        self.0.wait_default
    }

    #[must_use]
    pub fn wait_ceiling(&self) -> Duration {
        self.0.wait_ceiling
    }

    #[must_use]
    pub fn provider(&self) -> ProviderAttemptLimits {
        self.0.provider
    }

    /// The limit of one model call starting at `now_ms`: the model total,
    /// clipped to what remains of `enclosing` when the call is nested in
    /// another executable stretch (spec v3 L-C2). Its slice is one provider
    /// request.
    #[must_use]
    pub fn model_call_limit(
        &self,
        now_ms: u64,
        enclosing: Option<&ExecutionLimit>,
    ) -> ExecutionLimit {
        let own =
            ExecutionLimit::starting_at(now_ms, self.0.model_total, self.0.provider.per_request);
        match enclosing {
            Some(enclosing) => enclosing.nested(now_ms, own),
            None => own,
        }
    }

    /// The limit of one admission or checkpoint phase starting at `now_ms`.
    /// It bounds the whole phase, every check, store read and write inside it
    /// together, never each check alone (spec v3 L-C3).
    #[must_use]
    pub fn control_phase_limit(&self, now_ms: u64) -> ExecutionLimit {
        ExecutionLimit::starting_at(now_ms, self.0.control_phase, self.0.control_phase)
    }

    /// Admit a tool's declared execution against the inline ceiling (spec v3
    /// Part E), answering the bound of its inline execution.
    ///
    /// A tool that may only finish inline and declares more than
    /// [`tool_ceiling`](Self::tool_ceiling) is refused. An isolated,
    /// process-starting or Pending tool is admitted whatever it declares: its
    /// inline prefix is bounded by the ceiling, and the rest of its work
    /// waits under a wait deadline. Nothing is promoted at runtime.
    ///
    /// # Errors
    ///
    /// [`RegistrationRefused::InlineBudgetExceedsCeiling`].
    pub fn admit_tool(&self, manifest: &ToolManifest) -> Result<Duration, RegistrationRefused> {
        let declared = manifest.expected_execution.resolve(self.0.tool_default);
        if declared <= self.0.tool_ceiling {
            return Ok(declared);
        }
        if runs_long_work_outside_inline(&manifest.declaration) {
            return Ok(self.0.tool_ceiling);
        }
        Err(RegistrationRefused::InlineBudgetExceedsCeiling {
            tool: manifest.name.clone(),
            declared,
            ceiling: self.0.tool_ceiling,
            hint: INLINE_CEILING_HINT.to_string(),
        })
    }
}

const INLINE_CEILING_HINT: &str =
    "declare it as a process tool, an isolated tool, or a Pending tool that may defer";

/// Whether a declaration lets the tool's work continue past its inline
/// execution: an isolated call, a process-starting body, or a Pending body.
fn runs_long_work_outside_inline(declaration: &ToolDeclaration) -> bool {
    declaration.isolated
        || declaration.may_defer
        || declaration.intents.contains(&ToolIntentKind::StartProcess)
}

/// Why registration refused a tool.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum RegistrationRefused {
    /// An inline-only tool declares more execution than the ceiling allows.
    #[error(
        "tool `{tool}` declares {declared:?} of inline execution, above the {ceiling:?} ceiling: {hint}"
    )]
    InlineBudgetExceedsCeiling {
        tool: String,
        declared: Duration,
        ceiling: Duration,
        hint: String,
    },
}

/// The bound of one executable stretch: when it expires on lash's clock,
/// and the longest slice of it one attempt may run. It is minted once,
/// before the stretch starts, and never refreshed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionLimit {
    /// Milliseconds since the Unix epoch on lash's injected clock.
    pub expires_at: u64,
    pub max_slice: Duration,
}

impl ExecutionLimit {
    /// A limit of `total` from `now_ms`, sliced at most `max_slice` long.
    #[must_use]
    pub fn starting_at(now_ms: u64, total: Duration, max_slice: Duration) -> Self {
        let total_ms = u64::try_from(total.as_millis()).unwrap_or(u64::MAX);
        Self {
            expires_at: now_ms.saturating_add(total_ms),
            max_slice: max_slice.min(total),
        }
    }

    /// What remains of this limit at `now_ms`; zero once it has expired.
    #[must_use]
    pub fn remaining(&self, now_ms: u64) -> Duration {
        Duration::from_millis(self.expires_at.saturating_sub(now_ms))
    }

    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at
    }

    /// The longest the next slice may run from `now_ms`: the slice bound,
    /// clipped to what remains.
    #[must_use]
    pub fn slice(&self, now_ms: u64) -> Duration {
        self.max_slice.min(self.remaining(now_ms))
    }

    /// A stretch nested in this one: `own`, clipped to this limit's
    /// remaining time at `now_ms`.
    #[must_use]
    pub fn nested(&self, now_ms: u64, own: Self) -> Self {
        let expires_at = own.expires_at.min(self.expires_at);
        Self {
            expires_at,
            max_slice: own
                .max_slice
                .min(Duration::from_millis(expires_at.saturating_sub(now_ms))),
        }
    }
}

#[cfg(test)]
mod tests;
