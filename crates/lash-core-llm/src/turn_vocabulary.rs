//! Turn-execution vocabulary: the plain types and the phase-probe trait that
//! the plugin, tool-provider and tool-dispatch layers are written against.
//!
//! These carry no runtime machinery — no store handle, no engine handle, no
//! effect controller — so they sit below the layers that consume them.
//! `lash-core` re-exports every item here from `lash_core::runtime` at its
//! original path.

use std::sync::Arc;

macro_rules! define_runtime_turn_phases {
    ($($phase:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum RuntimeTurnPhase {
            $($phase),+
        }

        #[cfg(any(test, feature = "testing"))]
        impl RuntimeTurnPhase {
            pub const ALL: &'static [Self] = &[$(Self::$phase),+];
        }
    };
}

define_runtime_turn_phases!(
    ContextTransform,
    BeforeTurnHooks,
    PromptBuild,
    EffectLoop,
    PreparedTurn,
    CommittedTurn,
    PostCommitDelivery,
);

pub trait RuntimeTurnPhaseProbe: Send + Sync {
    fn begin(&self, phase: RuntimeTurnPhase);
    fn end(&self, phase: RuntimeTurnPhase);
    fn begin_named(&self, _phase: &str) {}
    fn end_named(&self, _phase: &str) {}
}

pub struct RuntimeNamedPhase {
    probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    phase: &'static str,
}

impl RuntimeNamedPhase {
    pub fn begin(
        probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
        phase: &'static str,
    ) -> RuntimeNamedPhase {
        if let Some(probe) = probe.as_ref() {
            probe.begin_named(phase);
        }
        RuntimeNamedPhase { probe, phase }
    }
}

impl Drop for RuntimeNamedPhase {
    fn drop(&mut self) {
        if let Some(probe) = self.probe.as_ref() {
            probe.end_named(self.phase);
        }
    }
}

/// Canonical assistant output payload.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AssistantOutput {
    pub safe_text: String,
    pub raw_text: String,
    pub state: OutputState,
}

/// Quality and usability of assembled terminal output.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputState {
    Usable,
    EmptyOutput,
    TracebackOnly,
    RecoveredFromError,
}

/// High-level execution summary for a completed turn.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TurnExecutionMetrics {
    #[serde(default)]
    pub had_tool_calls: bool,
    /// True when the turn emitted at least one code-block completion.
    #[serde(default)]
    pub had_code_execution: bool,
    /// Wall-clock turn start as epoch milliseconds, read from the runtime
    /// [`Clock`]. The measurement window opens when the runtime starts
    /// claiming the turn (session-execution lease / queued-work claim), so
    /// it covers the whole host-visible turn. `0` when the turn predates
    /// this field.
    #[serde(default)]
    pub started_at_ms: u64,
    /// Whole-turn duration in milliseconds — claim through final commit and
    /// post-persist hooks — measured on the runtime [`Clock`]'s monotonic
    /// source. `0` when the turn predates this field.
    #[serde(default)]
    pub duration_ms: u64,
}

/// Producer-selected effect of an issue on turn completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnIssueSeverity {
    /// Evidence that does not prevent completion.
    Advisory,
    /// A failure that prevents completion.
    Blocking,
}

/// Structured issue surfaced during turn execution.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnIssue {
    pub severity: TurnIssueSeverity,
    /// Typed origin of the failure, carrying the same wire spelling the field
    /// held as a bare `String`.
    pub kind: crate::TurnFailureKind,
    /// Typed failure code within `kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<crate::TurnFailureCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<crate::LlmTerminalReason>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Whether the failing operation is safe to retry, when the source
    /// carried a typed signal (provider transports classify retryability;
    /// terminal LLM responses are deterministic and report `Some(false)`).
    /// `None` means the source did not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Typed provider-failure classification, present only when the issue
    /// came from a classified LLM provider/transport failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_failure_kind: Option<crate::ProviderFailureKind>,
}
