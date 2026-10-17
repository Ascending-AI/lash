use lash_sansio::TurnId;

use crate::support::{
    Arc, LlmCallRecord, LlmUsage, ToolCallRecord, TurnActivity, TurnActivitySink,
    TurnExecutionMetrics, TurnOutcome, async_trait,
};
use crate::transcript::CommittedTurnWithToolCalls;

pub use lash_core::facade_support::{TurnIssue, TurnIssueSeverity};
/// Typed turn-failure vocabulary carried on [`TurnIssue`] and on session error
/// envelopes. A host branches on these instead of matching the display string.
/// The namespaced [`FailureCode`](crate::provider::FailureCode) on `code`
/// fields lives in [`crate::provider`].
pub use lash_core::{TurnFailureCode, TurnFailureKind};

/// A tool call addressed through the turn whose ledger contains it.
/// Ids are opaque; readers never recover a turn by searching session history.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct TurnToolCallRef {
    pub turn_id: TurnId,
    pub call_id: crate::ToolCallId,
}

/// A committed-turn page that opted into decoding each returned turn's calls.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommittedTurnsWithToolCallsPage {
    pub turns: Vec<CommittedTurnWithToolCalls>,
    /// Persist after applying every turn; continues the same committed feed.
    pub next: crate::persistence::CommittedTurnCursor,
}

/// A typed read of one session's recorded call. A body is exposed only
/// after its outcome commits; an admitted call without an outcome is pending.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolCallRead {
    Settled {
        #[serde(serialize_with = "crate::persistence::serialize_durable")]
        record: Box<ToolCallRecord>,
    },
    NotFound {
        call_id: crate::ToolCallId,
    },
    NotYetSettled {
        call_id: crate::ToolCallId,
    },
}

pub(crate) fn fresh_turn_id() -> TurnId {
    TurnId::from_uuid(uuid::Uuid::new_v4().as_u128())
}

/// A settled turn's result, without session state.
///
/// Read the committed head explicitly through
/// [`DurableSession::read`](crate::DurableSession::read) or
/// [`ObservableSession::snapshot`](crate::ObservableSession::snapshot).
/// Those reads answer the current committed head, which may include later
/// turns; this report remains the named run's result. For state as of a
/// settled run, use [`DurableSession::read_run`](crate::DurableSession::read_run).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct TurnReport {
    /// Cancellation evidence, when the turn was cancelled, rides this outcome
    /// — read it with [`TurnReport::cancellation`]. Runtime and provider
    /// stop causes ride the same terminal: read [`TurnReport::error_cause`].
    /// This evidence survives crashes and owner replacement.
    pub outcome: TurnOutcome,
    /// This session's own LLM tokens for the turn. Every session owns its
    /// usage; child-session tokens live on each child's own turn report.
    /// A durable report sums reported usage from the attempts in `llm_calls`;
    /// observation gaps can leave this sum incomplete.
    pub usage: LlmUsage,
    /// Provider calls made by the parent session during this turn, in protocol
    /// order. Child-session calls remain on each child's result. This is the
    /// complete lash-side model attribution surface: a turn has no single
    /// producing model, so lash exposes the per-call ledger and the host
    /// composes any higher-level view from it (ADR 0033).
    /// A durable follower retains only the sealed calls it observed; its gaps
    /// identify unavailable history.
    #[serde(default)]
    pub llm_calls: Vec<LlmCallRecord>,
    /// Bounded, non-transcript evidence from charge-safety-refused
    /// generations in this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<lash_core::TurnFailureEvidence>,
    /// Tool calls issued by the turn in protocol order.
    #[serde(serialize_with = "crate::persistence::serialize_durable")]
    pub tool_calls: Vec<ToolCallRecord>,
    /// Execution metadata collected for the turn.
    pub execution: TurnExecutionMetrics,
    /// Typed errors this follower observed while settling the turn. An
    /// observation gap can leave this list incomplete, as with `llm_calls`.
    /// Read [`TurnReport::error_cause`] for the authoritative terminal cause.
    pub errors: Vec<TurnIssue>,
    /// Durable acceptance identity of the input this turn was executed from.
    ///
    /// Every turn enters through one acceptance commit before anything executes
    /// (ADR 0069), and this is the same receipt
    /// [`SendHandle::receipt`](crate::SendHandle::receipt) answers: its
    /// `input_id` addresses the pending row and matches the settled application.
    /// Facade sessions always populate it because session open requires an
    /// explicitly selected store. The option remains for lower-level callers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<lash_core::runtime::TurnInputAcceptanceReceipt>,
    /// Undelivered inputs affected by this turn's cancellation policy.
    #[serde(
        default,
        skip_serializing_if = "lash_core::TurnCancelInputOutcome::is_empty"
    )]
    pub cancel_input_outcome: lash_core::TurnCancelInputOutcome,
    /// Where this report was assembled: from the turn as it ran in this
    /// process, or rebuilt from the session's durable state after it ran
    /// elsewhere.
    #[serde(default)]
    pub source: ReportSource,
}

/// Where a [`TurnReport`] came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportSource {
    /// Assembled from the turn as it ran in this process: every field is the
    /// turn's own account.
    #[default]
    Live,
    /// Assembled from the stored outcome and input acceptance.
    /// Tool records are read from the run's recorded calls. Model calls retain
    /// the sealed records this follower observed, and usage sums the reported
    /// usage of their attempts. Unavailable history is reported as gaps, so
    /// the model-call ledger, its usage sum and observed errors can be
    /// incomplete. Execution metrics are left at their default values.
    Durable,
}

impl TurnReport {
    /// A settled report of `outcome` and `usage` for a host that produces
    /// reports itself, such as a wire fixture or a projection test.
    ///
    /// The outcome is kept unchanged, with any cancellation evidence or error
    /// cause it carries. Every other fact starts empty: no call ledgers,
    /// failure evidence, observed errors, acceptance receipt or affected
    /// inputs, and default execution metrics. Empty evidence means none was
    /// supplied, not that none occurred. Name the report's provenance with
    /// `source`: [`ReportSource::Live`] for a turn's own account, or
    /// [`ReportSource::Durable`] for a report assembled from stored state.
    /// Supply the facts a test reads with the `with_*` methods.
    pub fn new(outcome: TurnOutcome, usage: LlmUsage, source: ReportSource) -> Self {
        Self {
            outcome,
            usage,
            llm_calls: Vec::new(),
            failure_evidence: Vec::new(),
            tool_calls: Vec::new(),
            execution: TurnExecutionMetrics::default(),
            errors: Vec::new(),
            acceptance: None,
            cancel_input_outcome: lash_core::TurnCancelInputOutcome::default(),
            source,
        }
    }

    /// The parent session's provider calls, in protocol order.
    #[must_use]
    pub fn with_llm_calls(mut self, llm_calls: Vec<LlmCallRecord>) -> Self {
        self.llm_calls = llm_calls;
        self
    }

    /// Evidence from charge-safety-refused generations.
    #[must_use]
    pub fn with_failure_evidence(
        mut self,
        failure_evidence: Vec<lash_core::TurnFailureEvidence>,
    ) -> Self {
        self.failure_evidence = failure_evidence;
        self
    }

    /// The turn's tool calls, in protocol order.
    #[must_use]
    pub fn with_tool_calls(mut self, tool_calls: Vec<ToolCallRecord>) -> Self {
        self.tool_calls = tool_calls;
        self
    }

    /// The turn's execution metrics.
    #[must_use]
    pub fn with_execution(mut self, execution: TurnExecutionMetrics) -> Self {
        self.execution = execution;
        self
    }

    /// Typed errors observed while settling the turn.
    #[must_use]
    pub fn with_errors(mut self, errors: Vec<TurnIssue>) -> Self {
        self.errors = errors;
        self
    }

    /// The acceptance receipt of the input the turn ran from.
    #[must_use]
    pub fn with_acceptance(
        mut self,
        acceptance: lash_core::runtime::TurnInputAcceptanceReceipt,
    ) -> Self {
        self.acceptance = Some(acceptance);
        self
    }

    /// Undelivered inputs the turn's cancellation policy affected.
    #[must_use]
    pub fn with_cancel_input_outcome(
        mut self,
        cancel_input_outcome: lash_core::TurnCancelInputOutcome,
    ) -> Self {
        self.cancel_input_outcome = cancel_input_outcome;
        self
    }

    /// The four-way status of the settled turn this report describes:
    /// [`Answered`](crate::TurnStatus::Answered),
    /// [`Failed`](crate::TurnStatus::Failed) or
    /// [`Cancelled`](crate::TurnStatus::Cancelled). A report exists only for
    /// a settled turn, so it is never [`Parked`](crate::TurnStatus::Parked).
    pub fn status(&self) -> crate::TurnStatus {
        crate::TurnStatus::of_outcome(&self.outcome)
    }

    /// Error cause committed with the terminal, available to live and durable
    /// readers even when this follower observed no errors.
    pub fn error_cause(&self) -> Option<&crate::TurnErrorCause> {
        self.outcome.error_cause()
    }

    /// Durable cancellation evidence, present exactly when this turn was
    /// cancelled. Cancellation evidence has no home other than the outcome.
    pub fn cancellation(&self) -> Option<&lash_core::facade_support::TurnCancellationEvidence> {
        self.outcome.cancellation()
    }

    /// Wall-clock instant the runtime started this turn (the
    /// session-execution lease take / queued-work admission), read from the runtime
    /// clock. Backed by [`TurnExecutionMetrics::started_at_ms`] on
    /// [`execution`](Self::execution).
    pub fn started_at(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.execution.started_at_ms)
    }

    /// Whole-turn duration — claim through final commit and post-persist
    /// hooks — measured on the runtime clock's monotonic source. Backed by
    /// [`TurnExecutionMetrics::duration_ms`] on [`execution`](Self::execution).
    pub fn duration(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.execution.duration_ms)
    }

    pub fn assistant_message(&self) -> Option<&str> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage {
                text,
            }) => Some(text),
            _ => None,
        }
    }

    /// The tool and the value a declared Finish control ended the turn
    /// with: `finish` for Lash's `control.finish`, or a host tool's name.
    pub fn finished(&self) -> Option<(&str, &serde_json::Value)> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::Finished {
                tool_name,
                value,
            }) => Some((tool_name.as_str(), value)),
            _ => None,
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(
            self.outcome,
            TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
        )
    }

    /// Returns whether the turn stopped because the assembled context
    /// exceeded the model's window.
    ///
    /// This is the recovery seam's read side: the outcome says the stop is
    /// recoverable by compaction rather than an undifferentiated provider
    /// failure. Acting on it is host policy — typically
    /// [`SessionStateAdmin::compact_context`](crate::admin::SessionStateAdmin::compact_context)
    /// followed by another turn on the same session. Lash chooses nothing.
    pub fn is_context_overflow(&self) -> bool {
        matches!(
            self.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ContextOverflow)
        )
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct TurnOutput {
    /// Final settled report for the turn.
    pub result: TurnReport,
    /// Ordered activities observed while executing the turn.
    pub activities: Vec<TurnActivity>,
}

impl TurnOutput {
    /// Combine a settled report with the activities collected by its observer.
    ///
    /// The activities describe that observer's view; they need not include
    /// every activity the run emitted. The report keeps its original source
    /// and authoritative outcome.
    pub fn new(result: TurnReport, activities: Vec<TurnActivity>) -> Self {
        Self { result, activities }
    }

    /// See [`TurnReport::status`].
    pub fn status(&self) -> crate::TurnStatus {
        self.result.status()
    }

    pub fn assistant_message(&self) -> Option<&str> {
        self.result.assistant_message()
    }

    /// See [`TurnReport::finished`].
    pub fn finished(&self) -> Option<(&str, &serde_json::Value)> {
        self.result.finished()
    }

    pub fn is_success(&self) -> bool {
        self.result.is_success()
    }

    /// See [`TurnReport::is_context_overflow`].
    pub fn is_context_overflow(&self) -> bool {
        self.result.is_context_overflow()
    }
}

/// Fans a turn's activity stream out to multiple consumers.
pub struct TurnActivityFanout {
    sinks: Vec<Arc<dyn TurnActivitySink>>,
}

impl TurnActivityFanout {
    pub fn new(sinks: impl IntoIterator<Item = Arc<dyn TurnActivitySink>>) -> Self {
        Self {
            sinks: sinks.into_iter().collect(),
        }
    }
}

#[async_trait]
impl TurnActivitySink for TurnActivityFanout {
    async fn emit(&self, activity: TurnActivity) {
        for sink in &self.sinks {
            sink.emit(activity.clone()).await;
        }
    }
}
