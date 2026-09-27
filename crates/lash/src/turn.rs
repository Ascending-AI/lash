use lash_sansio::TurnId;
use lash_sansio::sync::MutexExt;

use crate::support::{
    Arc, LlmCallRecord, LocalTurnStop, Message, MessageRole, RuntimeHandle, ScopedEffectController,
    SessionSnapshot, StdMutex, TokenUsage, ToolCallRecord, TurnActivity, TurnActivitySink,
    TurnExecutionMetrics, TurnOutcome, async_trait,
};

pub use lash_core::facade_support::{AssistantOutput, TurnIssue, TurnIssueSeverity};
/// Typed turn-failure vocabulary carried on [`TurnIssue`] and on session error
/// envelopes. A host branches on these instead of matching the display string.
/// The namespaced [`FailureCode`](crate::provider::FailureCode) on `code`
/// fields lives in [`crate::provider`].
pub use lash_core::{TurnFailureCode, TurnFailureKind};

pub(crate) fn fresh_turn_id() -> TurnId {
    TurnId::from(
        lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string())
            .0
            .to_string(),
    )
}

/// Drive `request` on `runtime`'s session to a stop through the session drive
/// (FIG-3600), recording every turn activity on the session's observation.
pub(crate) async fn drive_session_observed(
    runtime: &RuntimeHandle,
    binding: &lash_core::StoreBindingId,
    controller: &ScopedEffectController<'_>,
    request: &lash_core::engine::DriveRequest,
) -> std::result::Result<lash_core::engine::DriveOutcome, lash_core::engine::DriveAbort> {
    let writer_handle = runtime.writer();
    let mut writer = writer_handle.lock().await;
    let observation_sink = SessionObservationTurnActivitySink::new(runtime.clone(), None);
    let sinks = lash_core::drive::DriveSinks {
        events: &lash_core::runtime::NoopEventSink,
        turn_events: &observation_sink,
        local_stop: LocalTurnStop::default(),
    };
    let outcome =
        lash_core::drive::drive_session_reporting(&mut writer, controller, request, sinks).await;
    runtime.publish_from(&writer);
    let (outcome, roots) = outcome?;
    for root in roots {
        crate::send::deposit_settled_root(binding, &request.session, root);
    }
    Ok(outcome)
}

/// One recorded admission of `request` on `runtime`'s session (FIG-3600).
pub(crate) async fn admit_drive_observed(
    runtime: &RuntimeHandle,
    controller: &ScopedEffectController<'_>,
    request: &lash_core::engine::DriveRequest,
    ordinal: u32,
) -> std::result::Result<lash_core::engine::AdmitVerdict, lash_core::engine::DriveAbort> {
    let writer_handle = runtime.writer();
    let mut writer = writer_handle.lock().await;
    let verdict = lash_core::drive::admit_drive(&mut writer, controller, request, ordinal).await;
    runtime.publish_from(&writer);
    verdict
}

/// Run one admitted root on `runtime`'s session (FIG-3600), recording every
/// turn activity on the session's observation.
pub(crate) async fn run_admitted_root_observed(
    runtime: &RuntimeHandle,
    binding: &lash_core::StoreBindingId,
    controller: &ScopedEffectController<'_>,
    admitted: lash_core::engine::Admitted,
) -> std::result::Result<lash_core::engine::RootOutcome, lash_core::engine::DriveAbort> {
    let writer_handle = runtime.writer();
    let mut writer = writer_handle.lock().await;
    let session = admitted.session().clone();
    let observation_sink = SessionObservationTurnActivitySink::new(runtime.clone(), None);
    let sinks = lash_core::drive::DriveSinks {
        events: &lash_core::runtime::NoopEventSink,
        turn_events: &observation_sink,
        local_stop: LocalTurnStop::default(),
    };
    let report =
        lash_core::drive::run_admitted_root_reporting(&mut writer, controller, admitted, sinks)
            .await;
    runtime.publish_from(&writer);
    let report = report?;
    let outcome = report.outcome.clone();
    crate::send::deposit_settled_root(binding, &session, report);
    Ok(outcome)
}

/// Records every turn activity on the session's observation, addressed to
/// the physical turn that produced it, so a send handle can adopt the
/// activity of its input's root (FIG-3600 S5b). An activity published
/// without a turn is addressed to the last turn this sink saw.
pub(crate) struct SessionObservationTurnActivitySink<'a> {
    runtime: RuntimeHandle,
    live: Option<&'a dyn TurnActivitySink>,
    current_turn: StdMutex<Option<TurnId>>,
}

impl<'a> SessionObservationTurnActivitySink<'a> {
    pub(crate) fn new(runtime: RuntimeHandle, live: Option<&'a dyn TurnActivitySink>) -> Self {
        Self {
            runtime,
            live,
            current_turn: StdMutex::new(None),
        }
    }
}

#[async_trait]
impl TurnActivitySink for SessionObservationTurnActivitySink<'_> {
    fn is_noop(&self) -> bool {
        false
    }

    async fn emit(&self, activity: TurnActivity) {
        let current = self.current_turn.lock_recover().clone();
        self.runtime
            .record_turn_activity(current.as_ref(), activity.clone());
        if let Some(live) = self.live {
            live.emit(activity).await;
        }
    }

    async fn emit_for_turn(&self, turn_id: &TurnId, activity: TurnActivity) {
        *self.current_turn.lock_recover() = Some(turn_id.clone());
        self.runtime
            .record_turn_activity(Some(turn_id), activity.clone());
        if let Some(live) = self.live {
            live.emit_for_turn(turn_id, activity).await;
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnReport {
    /// Final runtime state for the turn.
    pub state: SessionSnapshot,
    /// Cancellation evidence, when the turn was cancelled, rides this outcome
    /// — read it with [`TurnReport::cancellation`].
    pub outcome: TurnOutcome,
    /// Assistant output committed by the turn.
    pub assistant_output: AssistantOutput,
    /// This session's own LLM tokens for the turn. Every session owns its
    /// usage; child-session tokens live on each child's own turn report.
    pub usage: TokenUsage,
    /// Provider calls made by the parent session during this turn, in protocol
    /// order. Child-session calls remain on each child's result. This is the
    /// complete lash-side model attribution surface: a turn has no single
    /// producing model, so lash exposes the per-call ledger and the host
    /// composes any higher-level view from it (ADR 0033).
    #[serde(default)]
    pub llm_calls: Vec<LlmCallRecord>,
    /// Bounded, non-transcript evidence from charge-safety-refused
    /// generations in this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<lash_core::TurnFailureEvidence>,
    /// Tool calls issued by the turn in protocol order.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Typed accounting for tool calls omitted from the bounded record view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<crate::OmittedToolCalls>,
    /// Execution metadata collected for the turn.
    pub execution: TurnExecutionMetrics,
    /// Errors captured while settling the turn.
    pub errors: Vec<TurnIssue>,
    /// Durable acceptance identity of the input this turn was driven from.
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
    /// Rebuilt from the session's durable state because the turn ran in
    /// another process (or its live report was no longer held): the outcome,
    /// the session state after it and the acceptance are the store's; the
    /// per-turn ledgers (usage, calls, tool records, execution metrics) are
    /// empty here and are read from the session's usage report and
    /// observation instead.
    Durable,
}

impl TurnReport {
    pub(crate) fn from_assembled(turn: lash_core::facade_support::AssembledTurn) -> Self {
        // Keep this exhaustive so adding a core turn-result field forces the
        // facade projection to be reviewed alongside the remote projection.
        let lash_core::facade_support::AssembledTurn {
            state,
            turn_input_acceptance,
            turn_cancel_input_outcome,
            outcome,
            assistant_output,
            execution,
            token_usage,
            llm_calls,
            tool_calls,
            omitted,
            failure_evidence,
            errors,
        } = turn;
        Self {
            state,
            outcome,
            assistant_output,
            usage: token_usage,
            llm_calls,
            failure_evidence,
            tool_calls,
            omitted,
            execution,
            errors,
            acceptance: turn_input_acceptance,
            cancel_input_outcome: turn_cancel_input_outcome,
            source: ReportSource::Live,
        }
    }

    /// This report as the remote protocol's settled turn, under `turn_id`
    /// (a send's root), carrying `activities` as its activity list. An
    /// activity the remote vocabulary cannot carry is left out.
    pub fn to_remote(
        &self,
        session_id: &lash_sansio::SessionId,
        turn_id: &TurnId,
        activities: &[TurnActivity],
    ) -> lash_remote_protocol::RemoteTurnReport {
        let turn = lash_core::facade_support::AssembledTurn {
            state: self.state.clone(),
            turn_input_acceptance: self.acceptance.clone(),
            turn_cancel_input_outcome: self.cancel_input_outcome.clone(),
            outcome: self.outcome.clone(),
            assistant_output: self.assistant_output.clone(),
            execution: self.execution.clone(),
            token_usage: self.usage.clone(),
            llm_calls: self.llm_calls.clone(),
            tool_calls: self.tool_calls.clone(),
            omitted: self.omitted.clone(),
            failure_evidence: self.failure_evidence.clone(),
            errors: self.errors.clone(),
        };
        let activities = activities
            .iter()
            .enumerate()
            .filter_map(|(sequence, activity)| {
                lash_remote_protocol::RemoteTurnActivity::from_core(
                    sequence as u64,
                    activity.clone(),
                )
                .ok()
            })
            .collect::<Vec<_>>();
        lash_remote_protocol::RemoteTurnReport::from_core(
            session_id.clone(),
            turn_id.clone(),
            turn,
            activities,
        )
    }

    /// The four-way status of the settled turn this report describes:
    /// [`Answered`](crate::TurnStatus::Answered),
    /// [`Failed`](crate::TurnStatus::Failed) or
    /// [`Cancelled`](crate::TurnStatus::Cancelled). A report exists only for
    /// a settled turn, so it is never [`Parked`](crate::TurnStatus::Parked).
    pub fn status(&self) -> crate::TurnStatus {
        crate::send::status_of_outcome(&self.outcome)
    }

    /// Durable cancellation evidence, present exactly when this turn was
    /// cancelled. Cancellation evidence has no home other than the outcome.
    pub fn cancellation(&self) -> Option<&lash_core::facade_support::TurnCancellationEvidence> {
        self.outcome.cancellation()
    }

    /// Wall-clock instant the runtime started this turn (claim of the
    /// session-execution lease / queued-work claim), read from the runtime
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

    pub fn final_value(&self) -> Option<&serde_json::Value> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { value }) => {
                Some(value)
            }
            _ => None,
        }
    }

    pub fn tool_value(&self) -> Option<(&str, &serde_json::Value)> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::ToolValue {
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
    /// [`SessionAdmin::compact_context`](crate::admin::SessionAdmin::compact_context)
    /// followed by another turn on the same session. Lash chooses nothing.
    pub fn is_context_overflow(&self) -> bool {
        matches!(
            self.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ContextOverflow)
        )
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnOutput {
    /// Final settled report for the turn.
    pub result: TurnReport,
    /// Ordered activities observed while executing the turn.
    pub activities: Vec<TurnActivity>,
}

impl TurnOutput {
    /// See [`TurnReport::status`].
    pub fn status(&self) -> crate::TurnStatus {
        self.result.status()
    }

    pub fn assistant_message(&self) -> Option<&str> {
        self.result.assistant_message()
    }

    pub fn final_value(&self) -> Option<&serde_json::Value> {
        self.result.final_value()
    }

    pub fn tool_value(&self) -> Option<(&str, &serde_json::Value)> {
        self.result.tool_value()
    }

    pub fn is_success(&self) -> bool {
        self.result.is_success()
    }

    /// See [`TurnReport::is_context_overflow`].
    pub fn is_context_overflow(&self) -> bool {
        self.result.is_context_overflow()
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct RunActivityCollector {
    activities: Arc<StdMutex<Vec<TurnActivity>>>,
}

#[cfg(test)]
impl RunActivityCollector {
    pub(crate) fn snapshot(&self) -> Vec<TurnActivity> {
        self.activities.lock_recover().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl TurnActivitySink for RunActivityCollector {
    async fn emit(&self, activity: TurnActivity) {
        self.activities.lock_recover().push(activity);
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

pub fn message_text(message: &Message) -> String {
    message
        .parts
        .iter()
        .map(|part| part.content())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn message_role(message: &Message) -> &'static str {
    match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Event => "event",
    }
}
