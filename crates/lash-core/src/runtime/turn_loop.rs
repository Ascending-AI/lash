#[cfg(test)]
use super::logical_turn::agent_frame_follow_turn_id;
use super::logical_turn::{
    LogicalTurnClaims, LogicalTurnStart, PhysicalTurnExecution, PreparedLogicalTurn,
};
use super::turn_control::ActiveTurnControl;
use super::*;
use crate::facade_support::{
    ProtocolTurnOptionsFacadeOps, RuntimeSessionStateFacadeOps, ScopedEffectControllerFacadeOps,
};
use lash_sansio::core_support::*;
use std::pin::Pin;

fn trace_fields_from_outcome(
    outcome: &TurnOutcome,
) -> (
    &'static str,
    &'static str,
    Option<lash_trace::TraceAgentFrameSwitch>,
) {
    match outcome {
        TurnOutcome::Finished(TurnFinish::AssistantMessage { .. }) => {
            ("completed", "assistant_message", None)
        }
        TurnOutcome::Finished(TurnFinish::FinalValue { .. }) => ("completed", "final_value", None),
        TurnOutcome::Finished(TurnFinish::ToolValue { .. }) => ("completed", "tool_value", None),
        TurnOutcome::AgentFrameSwitch { frame_id, .. } => (
            "completed",
            "agent_frame_switch",
            Some(lash_trace::TraceAgentFrameSwitch {
                frame_id: frame_id.clone(),
            }),
        ),
        TurnOutcome::Stopped(stop) => ("failed", trace_stop_reason(stop), None),
    }
}

fn trace_stop_reason(stop: &TurnStop) -> &'static str {
    match stop {
        TurnStop::Cancelled => "cancelled",
        TurnStop::Incomplete => "incomplete",
        TurnStop::InvalidInput => "invalid_input",
        TurnStop::MaxTurns => "max_turns",
        TurnStop::ToolFailure => "tool_failure",
        TurnStop::ProviderError => "provider_error",
        TurnStop::PluginAbort => "plugin_abort",
        TurnStop::RuntimeError => "runtime_error",
        TurnStop::SubmittedError { .. } => "submitted_error",
        TurnStop::ToolError { .. } => "tool_error",
    }
}

pub(super) fn post_commit_delivery_issue(
    code: impl Into<String>,
    message: impl Into<String>,
) -> TurnIssue {
    TurnIssue {
        kind: "runtime".to_string(),
        code: Some(code.into()),
        terminal_reason: None,
        message: message.into(),
        raw: None,
        retryable: Some(false),
        provider_failure_kind: None,
    }
}

fn session_head_refresh_error(err: SessionError) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::SessionHeadRefresh, err.to_string())
}

#[derive(Clone, Copy)]
pub(super) enum SessionExecutionLeaseReleasePolicy {
    KeepOnAgentFrameSwitch,
}

impl SessionExecutionLeaseReleasePolicy {
    fn should_release(self, outcome: &TurnOutcome) -> bool {
        match self {
            Self::KeepOnAgentFrameSwitch => {
                !matches!(outcome, TurnOutcome::AgentFrameSwitch { .. })
            }
        }
    }
}

fn queued_work_payload_type(payload: &crate::QueuedWorkPayload) -> &'static str {
    match payload {
        crate::QueuedWorkPayload::ProcessWake { .. } => "process_wake",
        crate::QueuedWorkPayload::AgentFrameTask { .. } => "agent_frame_task",
        crate::QueuedWorkPayload::SessionCommand { command } => command.kind(),
    }
}

fn queued_work_batch_ids(claim: &crate::QueuedWorkClaim) -> Vec<String> {
    claim
        .batches
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect()
}

/// Measures the whole host-visible turn.
///
/// Opened before the runtime claims the turn (session-execution lease and
/// queued-work/turn-input claims) and stamped onto the assembled turn after
/// the final commit and post-persist hooks complete, so
/// [`ExecutionSummary`](crate::ExecutionSummary) timing covers
/// claim → final commit. Reads only the injected [`Clock`](crate::Clock):
/// `started_at_ms` comes from the wall-clock source and the duration from the
/// monotonic source, so deterministic clocks produce deterministic timing.
#[derive(Clone, Copy)]
pub(super) struct TurnStopwatch {
    started: std::time::Instant,
    started_at_ms: u64,
}

impl TurnStopwatch {
    pub(super) fn start(clock: &dyn crate::Clock) -> Self {
        Self {
            started: clock.now(),
            started_at_ms: clock.timestamp_ms(),
        }
    }

    pub(super) fn stamp(&self, turn: &mut AssembledTurn, clock: &dyn crate::Clock) {
        turn.execution.started_at_ms = self.started_at_ms;
        turn.execution.duration_ms = clock
            .now()
            .saturating_duration_since(self.started)
            .as_millis() as u64;
    }
}

fn turn_phase_id(parent_turn_id: &str, phase: &str) -> String {
    format!("{parent_turn_id}:{phase}")
}

fn scoped_child_turn_controller<'run>(
    scoped_effect_controller: &ScopedEffectController<'run>,
    session_id: &str,
    turn_id: &str,
) -> Result<ScopedEffectController<'run>, RuntimeError> {
    let scope = ExecutionScope::turn(session_id, turn_id);
    scoped_effect_controller.rescope(scope)
}

/// Select the resolver that owns turn-control promises for this deployment.
///
/// Runtime-owned promise identity is instance-owned, so every control operation
/// must use the configured host that [`TurnWorkDriver`] addresses. Controllers
/// that own replay use the run-scoped controller so control reads and writes
/// remain replay-aware while the deployment host supplies the live watch.
fn turn_control_resolver<'a>(
    effect_host: &'a dyn EffectHost,
    scoped_effect_controller: &'a ScopedEffectController<'_>,
) -> &'a dyn AwaitEventResolver {
    if effect_host.replay_ownership() == crate::EffectReplayOwnership::Runtime {
        effect_host
    } else {
        scoped_effect_controller.controller()
    }
}

pub(in crate::runtime) fn queued_work_trace_payload(
    boundary: crate::QueuedWorkClaimBoundary,
    claim: &crate::QueuedWorkClaim,
    causes: &[crate::TurnCause],
) -> serde_json::Value {
    serde_json::json!({
        "boundary": boundary,
        "claim_id": claim.claim_id,
        "owner_id": claim.owner.owner_id,
        "incarnation_id": claim.owner.incarnation_id,
        "batch_ids": queued_work_batch_ids(claim),
        "payload_types": claim.batches.iter()
            .flat_map(|batch| batch.items.iter())
            .map(|item| queued_work_payload_type(&item.payload))
            .collect::<Vec<_>>(),
        "causes": causes,
    })
}

pub(in crate::runtime) fn queued_work_completion_trace_payload(
    completions: &[crate::QueuedWorkCompletion],
) -> serde_json::Value {
    serde_json::json!({
        "claims": completions.iter().map(|completion| {
            serde_json::json!({
                "session_id": completion.session_id,
                "claim_id": completion.claim_id,
                "batch_ids": completion.batch_ids,
            })
        }).collect::<Vec<_>>(),
    })
}

pub(in crate::runtime) fn turn_input_completion_trace_payload(
    completions: &[crate::TurnInputCompletion],
) -> serde_json::Value {
    serde_json::json!({
        "claims": completions.iter().map(|completion| {
            serde_json::json!({
                "session_id": completion.session_id,
                "claim_id": completion.claim_id,
                "input_ids": completion.input_ids,
            })
        }).collect::<Vec<_>>(),
    })
}

async fn emit_queued_work_started_to_sink(
    events: &dyn TurnActivitySink,
    turn_id: &str,
    boundary: crate::QueuedWorkClaimBoundary,
    claim: &crate::QueuedWorkClaim,
    causes: Vec<crate::TurnCause>,
) {
    emit_turn_activity_to_sink_for_turn(
        events,
        turn_id,
        TurnActivity::independent(TurnEvent::QueuedWorkStarted {
            boundary,
            batch_ids: queued_work_batch_ids(claim),
            causes,
        }),
    )
    .await;
}

pub(in crate::runtime) async fn send_queued_work_started_event(
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    boundary: crate::QueuedWorkClaimBoundary,
    claim: &crate::QueuedWorkClaim,
    causes: Vec<crate::TurnCause>,
) {
    send_turn_activity(
        event_tx,
        TurnActivityId::new(uuid::Uuid::new_v4().to_string()),
        TurnEvent::QueuedWorkStarted {
            boundary,
            batch_ids: queued_work_batch_ids(claim),
            causes,
        },
    )
    .await;
}

struct TurnFinishInput {
    turn_pipeline: TurnBoundary,
    assembler: TurnAssembler,
    new_messages: crate::MessageSequence,
    policy: RuntimeSessionPolicy,
    turn_index: usize,
    trace_turn_id: String,
}

trait TypedTurnPhase {
    const RUNTIME_PHASE: RuntimeTurnPhase;
}

struct PreparedTurn {
    turn_pipeline: TurnBoundary,
    turn: AssembledTurn,
    events: Vec<SessionStreamEvent>,
}

impl PreparedTurn {
    fn outcome(&self) -> &TurnOutcome {
        &self.turn.outcome
    }

    fn final_operation(&self) -> crate::OperationId {
        self.turn_pipeline.final_operation()
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit(
        mut self,
        session: Option<&mut Session>,
        staged_usage: session_manager::StagedTokenLedger,
        commit_effects: super::logical_turn::LogicalTurnCommitEffects,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        release_session_execution_lease: bool,
        trace_turn_id: &str,
    ) -> Result<CommittedTurn, crate::StoreError> {
        let accepted = self
            .turn_pipeline
            .final_commit(
                &mut self.turn,
                session,
                staged_usage.deltas(),
                commit_effects.originating_queue_claims,
                commit_effects.originating_turn_input_claims,
                commit_effects.completed_queue_claims,
                commit_effects.completed_turn_input_claims,
                commit_effects.queue_claim_generations,
                commit_effects.turn_input_claim_generations,
                session_execution_lease.map(|lease| lease.fence().fencing_token),
                commit_effects.enqueued_queue_batches,
                // Any active-turn input that missed the turn's final
                // checkpoint must become the next ordinary user turn.
                Some(trace_turn_id.to_string()),
                release_session_execution_lease
                    .then(|| session_execution_lease.map(SessionExecutionLeaseGuard::completion))
                    .flatten(),
            )
            .await?;
        Ok(CommittedTurn {
            turn: self.turn,
            events: self.events,
            resident_state: self.turn_pipeline.into_final_state(),
            accepted,
            staged_usage,
            release_session_execution_lease,
            retained_lease_continuity: if release_session_execution_lease {
                None
            } else {
                session_execution_lease.and_then(SessionExecutionLeaseGuard::continuity)
            },
        })
    }
}

impl TypedTurnPhase for PreparedTurn {
    const RUNTIME_PHASE: RuntimeTurnPhase = RuntimeTurnPhase::PreparedTurn;
}

struct CommittedTurn {
    turn: AssembledTurn,
    events: Vec<SessionStreamEvent>,
    resident_state: RuntimeSessionState,
    accepted: AcceptedTurnCommit,
    staged_usage: session_manager::StagedTokenLedger,
    release_session_execution_lease: bool,
    retained_lease_continuity: Option<SessionExecutionLeaseContinuity>,
}

impl TypedTurnPhase for CommittedTurn {
    const RUNTIME_PHASE: RuntimeTurnPhase = RuntimeTurnPhase::CommittedTurn;
}

impl CommittedTurn {
    /// Synchronize the accepted durable commit into the resident runtime.
    /// This transition intentionally cannot await; consuming `self` is the
    /// only way to obtain the post-commit delivery phase.
    fn adopt(
        self,
        runtime: &mut LashRuntime,
        trace_turn_id: &str,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<PostCommitDelivery, crate::StoreError> {
        let (enqueued_queue_batches, confirmed_usage) = self.accepted.into_parts();
        self.staged_usage.confirm_identities(&confirmed_usage)?;
        if self.release_session_execution_lease
            && let Some(lease) = session_execution_lease
        {
            lease.mark_released();
        }
        runtime.last_committed_lease_continuity = self.retained_lease_continuity;
        runtime.state = self.resident_state;
        let observation_revision = if runtime.state.checkpoint_ref.is_some() {
            runtime.state.head_revision
        } else {
            runtime.state.turn_index as u64
        };
        runtime.last_committed_observation_turn =
            Some((observation_revision, trace_turn_id.to_string()));
        Ok(PostCommitDelivery {
            turn: self.turn,
            events: self.events,
            enqueued_queue_batches,
            post_commit_delivery_failed: false,
        })
    }
}

struct PostCommitDelivery {
    turn: AssembledTurn,
    events: Vec<SessionStreamEvent>,
    enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
    post_commit_delivery_failed: bool,
}

impl TypedTurnPhase for PostCommitDelivery {
    const RUNTIME_PHASE: RuntimeTurnPhase = RuntimeTurnPhase::PostCommitDelivery;
}

struct TurnDriverSessionLoan<'slot, 'run> {
    slot: &'slot mut Option<Session>,
    driver: Option<Box<RuntimeTurnDriver<'run>>>,
}

impl<'slot, 'run> TurnDriverSessionLoan<'slot, 'run> {
    fn new(slot: &'slot mut Option<Session>, driver: Box<RuntimeTurnDriver<'run>>) -> Self {
        Self {
            slot,
            driver: Some(driver),
        }
    }

    fn into_inner(mut self) -> Box<RuntimeTurnDriver<'run>> {
        self.driver.take().expect("turn driver loan is present")
    }
}

impl<'slot, 'run> std::ops::Deref for TurnDriverSessionLoan<'slot, 'run> {
    type Target = RuntimeTurnDriver<'run>;

    fn deref(&self) -> &Self::Target {
        self.driver.as_deref().expect("turn driver loan is present")
    }
}

impl std::ops::DerefMut for TurnDriverSessionLoan<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.driver
            .as_deref_mut()
            .expect("turn driver loan is present")
    }
}

impl Drop for TurnDriverSessionLoan<'_, '_> {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            *self.slot = Some(driver.session);
        }
    }
}

impl LashRuntime {
    pub(super) fn invalidate_resident_session_state(&mut self) {
        if self.resident_session_state_valid {
            self.resident_session_reload_decision_id = Some(format!(
                "resident-session-reload:{}:{}",
                self.state.session_id,
                uuid::Uuid::new_v4()
            ));
        }
        self.resident_session_state_valid = false;
        self.graph_loaded_from_store = false;
        self.last_committed_lease_continuity = None;
        self.last_committed_observation_turn = None;
        if let Some(session) = self.session.as_ref() {
            session.invalidate_runtime_caches();
        }
    }

    pub(super) fn trace_synchronous_resident_state_refusal(&self, consumer: &'static str) {
        tracing::info!(
            event = "resident_session_state.sync_refusal",
            decision_id = self
                .resident_session_reload_decision_id
                .as_deref()
                .unwrap_or("resident-session-reload:missing"),
            session_id = %self.state.session_id,
            consumer,
            consulted_validity = self.resident_session_state_valid,
            outcome = "refused",
            error_classification = RuntimeErrorCode::ResidentSessionReloadFailed.as_str(),
            "synchronous resident-state consumer refused invalidated state"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn trace_resident_session_reload_decision(
        &self,
        decision_id: &str,
        consulted_validity: bool,
        durable_source: &'static str,
        resident_head_revision: u64,
        durable_head_freshness: &'static str,
        durable_head_revision: u64,
        failing_restore_stage: &'static str,
        outcome: &'static str,
        error_classification: &str,
    ) {
        tracing::info!(
            event = "resident_session_state.reload_decision",
            decision_id,
            session_id = %self.state.session_id,
            consulted_validity,
            durable_source,
            resident_head_revision,
            durable_head_freshness,
            durable_head_revision,
            failing_restore_stage,
            outcome,
            error_classification,
            "resident-state reload gate decided"
        );
    }

    pub(super) async fn reload_invalidated_resident_session_state(
        &mut self,
    ) -> Result<(), RuntimeError> {
        if self.resident_session_state_valid {
            self.trace_resident_session_reload_decision(
                "resident-session-reload:not-required",
                true,
                "not_consulted",
                self.state.head_revision,
                "current_resident_state",
                self.state.head_revision,
                "none",
                "not_required",
                "none",
            );
            return Ok(());
        }

        let decision_id = self
            .resident_session_reload_decision_id
            .clone()
            .unwrap_or_else(|| "resident-session-reload:missing".to_string());
        let resident_head_revision = self.state.head_revision;
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store());
        let durable_source = if store.is_some() {
            "history_store"
        } else {
            "resident_snapshot"
        };
        let mut durable_head_freshness = if store.is_some() {
            "refresh_pending"
        } else {
            "store_unavailable"
        };
        let mut durable_state = self.state.clone();
        let mut durable_head_revision = durable_state.head_revision;
        let reload_result: Result<(), (&'static str, RuntimeError)> = async {
            if let Some(store) = store.as_ref() {
                crate::store::refresh_persisted_session_state(store.as_ref(), &mut durable_state)
                    .await
                    .map_err(|err| {
                        (
                            "durable_head_refresh",
                            RuntimeError::new(
                                RuntimeErrorCode::ResidentSessionReloadFailed,
                                format!(
                                    "failed to reload invalidated resident session state: {err}"
                                ),
                            ),
                        )
                    })?;
                durable_head_freshness = "reloaded_from_store";
                durable_head_revision = durable_state.head_revision;
            }

            let session = self.session.as_mut().ok_or_else(|| {
                (
                    "session_availability",
                    RuntimeError::new(
                        RuntimeErrorCode::ResidentSessionReloadFailed,
                        "runtime session is unavailable while reloading invalidated resident state",
                    ),
                )
            })?;
            session.invalidate_runtime_caches();
            if let Some(tool_state) = durable_state.tool_state_snapshot().cloned() {
                session
                    .plugins()
                    .tool_registry()
                    .restore_state(tool_state)
                    .map_err(|err| {
                        (
                            "tool_state_restore",
                            RuntimeError::new(
                                RuntimeErrorCode::ResidentSessionReloadFailed,
                                err.to_string(),
                            ),
                        )
                    })?;
            }
            session.refresh_tool_catalog().await.map_err(|err| {
                (
                    "tool_catalog_refresh",
                    RuntimeError::new(
                        RuntimeErrorCode::ResidentSessionReloadFailed,
                        err.to_string(),
                    ),
                )
            })?;
            if let Some(snapshot) = durable_state.plugin_snapshot() {
                session.plugins().restore(snapshot).map_err(|err| {
                    (
                        "plugin_snapshot_restore",
                        RuntimeError::new(
                            RuntimeErrorCode::ResidentSessionReloadFailed,
                            err.to_string(),
                        ),
                    )
                })?;
            }
            let protocol_session = Arc::clone(session.plugins().protocol_session());
            let session_id = durable_state.session_id.clone();
            protocol_session
                .restore_session(
                    crate::plugin::ProtocolSessionContext::new(session, &session_id),
                    &durable_state,
                )
                .await
                .map_err(|err| {
                    (
                        "protocol_session_restore",
                        RuntimeError::new(
                            RuntimeErrorCode::ResidentSessionReloadFailed,
                            err.to_string(),
                        ),
                    )
                })?;

            durable_state.discard_runtime_snapshots();
            session
                .plugins()
                .emit_runtime_event(crate::PluginLifecycleEvent::SessionRestored(
                    crate::SessionReadView::from_persisted_state(&durable_state),
                ))
                .await
                .map_err(|err| {
                    (
                        "session_restored_hook",
                        RuntimeError::new(
                            RuntimeErrorCode::ResidentSessionReloadFailed,
                            err.to_string(),
                        ),
                    )
                })?;
            self.policy = durable_state.effective_policy().clone();
            self.protocol_turn_options = durable_state.effective_protocol_turn_options().clone();
            self.state = durable_state;
            self.graph_loaded_from_store = false;
            self.resident_session_state_valid = true;
            Ok(())
        }
        .await;

        match reload_result {
            Ok(()) => {
                self.trace_resident_session_reload_decision(
                    &decision_id,
                    false,
                    durable_source,
                    resident_head_revision,
                    durable_head_freshness,
                    durable_head_revision,
                    "none",
                    "restored",
                    "none",
                );
                Ok(())
            }
            Err((failing_restore_stage, err)) => {
                if failing_restore_stage == "durable_head_refresh" {
                    durable_head_freshness = "refresh_failed";
                }
                self.trace_resident_session_reload_decision(
                    &decision_id,
                    false,
                    durable_source,
                    resident_head_revision,
                    durable_head_freshness,
                    durable_head_revision,
                    failing_restore_stage,
                    "denied",
                    err.code.as_str(),
                );
                Err(err)
            }
        }
    }

    pub(super) async fn reload_invalidated_resident_session_state_for_session(
        &mut self,
    ) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state()
            .await
            .map_err(|err| SessionError::Protocol(err.to_string()))
    }

    pub(super) fn max_context_tokens(&self) -> usize {
        self.state.effective_policy().context_window_tokens()
    }

    /// Claim the lane for this turn, or record why the turn proceeds without it.
    ///
    /// A busy lane does not stop the turn: the commit CAS is the authority
    /// (ADR 0029), so the turn continues and may well win. It must not continue
    /// *anonymously* though, or a later `commit_cas_rejected` cannot say who was
    /// writing under whose generation, so the observed holder is retained as
    /// lane-less commit evidence for this turn.
    async fn claim_session_execution_lease(
        &mut self,
    ) -> Result<Option<SessionExecutionLeaseGuard>, RuntimeError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(None);
        };
        match SessionExecutionLeaseGuard::try_acquire(
            store,
            &self.state.session_id,
            &self.runtime_lease_owner,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()))?
        {
            Some(guard) => Ok(Some(guard)),
            None => {
                // The claim itself already logged the holder it lost to
                // (`session_execution_lease.busy`). The turn proceeds without the
                // lane because the commit CAS is the authority (ADR 0029), and a
                // rejection still names this writer from its claimant identity.
                tracing::debug!(
                    session_id = %self.state.session_id,
                    consulted = "session_execution_lease",
                    outcome = "proceeding_under_commit_cas",
                    event = "session_execution_lease.busy_advisory",
                    "session execution lease is busy; proceeding under the commit CAS fence"
                );
                Ok(None)
            }
        }
    }

    async fn settle_session_execution_lease<T>(
        &self,
        guard: Option<&SessionExecutionLeaseGuard>,
        result: Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        match result {
            Ok(value) => {
                if let Some(guard) = guard {
                    guard.release_if_live().await.map_err(|err| {
                        RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
                    })?;
                }
                Ok(value)
            }
            Err(err) => {
                if err.code != RuntimeErrorCode::StoreCommitFailed
                    && let Some(guard) = guard
                    && let Err(release_err) = guard.release_if_live().await
                {
                    tracing::warn!(
                        error = %release_err,
                        "failed to release session execution lease after runtime error"
                    );
                }
                Err(err)
            }
        }
    }

    // Prompt handback after an operation observes lease loss or an unambiguous
    // local pre-commit capture abort. Abandon clears
    // claim ownership, which both frees the rows for a peer and invalidates this
    // owner's pending completion. That is safe here because the turn is already
    // failing on the observed lease loss (ADR 0029).
    async fn abandon_queued_work_claims_after_local_abort(
        &self,
        err: &RuntimeError,
        claims: &[crate::QueuedWorkClaim],
    ) {
        if !matches!(
            err.code,
            RuntimeErrorCode::SessionExecutionLeaseLost
                | RuntimeErrorCode::ExecutionStateCaptureFailed
        ) || claims.is_empty()
        {
            return;
        }
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return;
        };
        if let Err(abandon_err) = store.abandon_queued_work_claims(claims).await {
            tracing::warn!(
                error = %abandon_err,
                claim_count = claims.len(),
                "failed to abandon queued work claims after local turn abort"
            );
        }
    }

    async fn abandon_turn_input_claims_after_local_abort(
        &self,
        err: &RuntimeError,
        claims: &[crate::TurnInputClaim],
    ) {
        if !matches!(
            err.code,
            RuntimeErrorCode::SessionExecutionLeaseLost
                | RuntimeErrorCode::ExecutionStateCaptureFailed
        ) || claims.is_empty()
        {
            return;
        }
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return;
        };
        if let Err(abandon_err) = store.abandon_turn_input_claims(claims).await {
            tracing::warn!(
                error = %abandon_err,
                claim_count = claims.len(),
                "failed to abandon turn input claims after local turn abort"
            );
        }
    }

    #[doc(hidden)]
    pub fn set_turn_phase_probe(&mut self, probe: Arc<dyn RuntimeTurnPhaseProbe>) {
        self.turn_phase_probe = Some(probe);
    }

    fn mark_phase_begin(&self, phase: RuntimeTurnPhase) {
        if let Some(probe) = self.turn_phase_probe.as_ref() {
            probe.begin(phase);
        }
    }

    fn mark_phase_end(&self, phase: RuntimeTurnPhase) {
        if let Some(probe) = self.turn_phase_probe.as_ref() {
            probe.end(phase);
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_turn(
        &mut self,
        finish: TurnFinishInput,
        claims: &LogicalTurnClaims,
        events: &dyn EventSink,
        scoped_effect_controller: &ScopedEffectController<'_>,
        cancel_state: &CancellationToken,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        session_execution_lease_release_policy: SessionExecutionLeaseReleasePolicy,
        turn_control: &ActiveTurnControl,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_resolver =
            turn_control_resolver(turn_control_host.as_ref(), scoped_effect_controller);
        let TurnFinishInput {
            mut turn_pipeline,
            assembler,
            new_messages,
            policy,
            turn_index,
            trace_turn_id,
        } = finish;
        self.policy = self.state.effective_policy().clone();
        turn_pipeline.state_mut().policy = self.policy.clone();
        turn_pipeline.state_mut().turn_index = turn_index;

        if !assembler.token_usage.is_zero() {
            session_manager::record_token_usage_shared(
                &self.shared_token_ledger,
                "turn",
                &policy.model.id,
                &assembler.token_usage,
            );
        }

        let assembled_cancelled = matches!(
            assembler.outcome,
            Some(TurnOutcome::Stopped(TurnStop::Cancelled))
        );
        let lease_was_lost = session_execution_lease.is_some_and(|lease| lease.is_lost());
        let cancellation = turn_control
            .settle_before_commit(
                turn_control_resolver,
                assembled_cancelled || (cancel_state.is_cancelled() && !lease_was_lost),
            )
            .await?;
        if cancellation.is_some() {
            cancel_state.cancel();
        }
        // Interruption derives from sealed evidence, never the raw token: a
        // lease-loss wakeup cancels the token without evidence and must not
        // become a Cancelled outcome. When a durable cancel races lease loss,
        // the final commit's head CAS and any claim batch-ownership checks are
        // the arbiters. Lease loss alone does not reject a current-head commit.
        let interrupted = cancellation.is_some();

        turn_pipeline.finalize_turn_read_state(new_messages, interrupted);
        for diagnostic in turn_pipeline.take_projection_diagnostics() {
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                lash_trace::TraceContext::default()
                    .for_session(self.state.session_id.clone())
                    .for_turn_index(turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::Custom {
                    name: "session_graph.read_projection".to_string(),
                    payload: serde_json::json!({
                        "durably_appended_messages": diagnostic.durably_appended_messages,
                        "observation_only_messages": diagnostic.observation_only_messages,
                        "id_mismatch_message_ids": diagnostic.id_mismatches,
                    }),
                },
                self.host.core.clock.as_ref(),
            );
        }
        if !assembler.token_usage.is_zero() {
            turn_pipeline.state_mut().token_usage = assembler.token_usage.clone();
        }

        let last_prompt_usage = assembler.last_llm_usage().and_then(normalize_prompt_usage);
        turn_pipeline.state_mut().last_prompt_usage = last_prompt_usage;
        let assembled_state = turn_pipeline.export_state_for_assembly();
        let mut assembled = assembler.finish(
            assembled_state,
            interrupted,
            None,
            &self.host.core.control.termination,
        );
        assembled.cancellation = cancellation;

        let Some(session) = self.session.as_ref() else {
            self.state.apply_snapshot(&assembled.state);
            let observation_revision = if self.state.checkpoint_ref.is_some() {
                self.state.head_revision
            } else {
                self.state.turn_index as u64
            };
            self.last_committed_observation_turn =
                Some((observation_revision, trace_turn_id.clone()));
            self.emit_completed_turn_trace(&assembled.state, &assembled.outcome, &trace_turn_id);
            publish_terminal_after_commit(
                turn_control,
                turn_control_resolver,
                &TurnTerminal::Committed {
                    outcome: assembled.outcome.clone(),
                    cancellation: assembled.cancellation.clone(),
                    session_revision: None,
                },
                &self.state.session_id,
                &trace_turn_id,
            )
            .await;
            return Ok(PhysicalTurnExecution {
                turn: assembled,
                enqueued_queue_batches: Vec::new(),
                post_commit_delivery_failed: false,
            });
        };

        let plugins = Arc::clone(session.plugins());
        let manager = match self.runtime_session_services_for_turn(None, session_execution_lease) {
            Ok(manager) => manager,
            Err(err) => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::PluginSessionManager,
                    err.to_string(),
                ));
            }
        };

        self.mark_phase_begin(PreparedTurn::RUNTIME_PHASE);
        let finalized = match plugins
            .finalize_turn_with_phase_probe(
                assembled,
                manager.state_service(),
                manager.lifecycle_service(),
                manager.graph_service(),
                self.turn_phase_probe.clone(),
                &trace_turn_id,
            )
            .await
        {
            Ok(finalized) => finalized,
            Err(err) => {
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(RuntimeError::new(
                    RuntimeErrorCode::PluginFinalizeTurn,
                    err.to_string(),
                ));
            }
        };
        let mut returned_turn = finalized.turn;
        if returned_turn.cancellation.is_some()
            && !matches!(
                returned_turn.outcome,
                TurnOutcome::Stopped(TurnStop::Cancelled)
            )
        {
            returned_turn.outcome = TurnOutcome::Stopped(TurnStop::Cancelled);
        }
        if matches!(
            returned_turn.outcome,
            TurnOutcome::Stopped(TurnStop::Cancelled)
        ) && returned_turn.cancellation.is_none()
        {
            self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::TurnCancellationEvidenceMissing,
                "cancelled turns must carry cancellation evidence",
            ));
        }
        let prepared = PreparedTurn {
            turn_pipeline,
            turn: returned_turn,
            events: finalized.events,
        };
        let release_session_execution_lease =
            session_execution_lease_release_policy.should_release(prepared.outcome());
        let commit_effects = claims.commit_effects(
            prepared.outcome(),
            &self.state.session_id,
            &trace_turn_id,
            Some(self.state.effective_protocol_turn_options().clone()),
        );
        let queued_work_completion_trace = commit_effects.completed_queue_claims.clone();
        let turn_input_completion_trace = commit_effects.completed_turn_input_claims.clone();
        let staged_usage = match session_manager::stage_token_ledger_shared(
            &self.shared_token_ledger,
            &prepared.final_operation(),
        ) {
            Ok(staged_usage) => staged_usage,
            Err(err) => {
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(runtime_error_from_store_commit(err));
            }
        };
        let committed = match prepared
            .commit(
                self.session.as_mut(),
                staged_usage,
                commit_effects,
                session_execution_lease,
                release_session_execution_lease,
                &trace_turn_id,
            )
            .await
        {
            Ok(committed) => committed,
            Err(err) => {
                crate::trace::emit_store_error(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    lash_trace::TraceContext::default()
                        .for_session(self.state.session_id.clone())
                        .for_turn(trace_turn_id.clone()),
                    "turn_commit",
                    &err,
                    self.host.core.clock.as_ref(),
                );
                // Reported here, not inside the commit: the guard reference and the
                // claimant are already live in this future, so naming the writer
                // costs nothing, while carrying evidence through the commit await
                // would grow every turn future.
                trace_commit_cas_rejected(
                    &self.state.session_id,
                    session_execution_lease
                        .map(SessionExecutionLeaseGuard::commit_evidence)
                        .as_deref(),
                    &self.runtime_lease_owner,
                    &err,
                );
                self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
                return Err(runtime_error_from_store_commit(err));
            }
        };
        self.mark_phase_end(PreparedTurn::RUNTIME_PHASE);
        self.mark_phase_begin(CommittedTurn::RUNTIME_PHASE);
        let mut delivery = committed
            .adopt(self, &trace_turn_id, session_execution_lease)
            .map_err(runtime_error_from_store_commit)?;
        self.mark_phase_end(CommittedTurn::RUNTIME_PHASE);
        self.mark_phase_begin(PostCommitDelivery::RUNTIME_PHASE);

        emit_session_events_to_sink(events, delivery.events).await;
        publish_terminal_after_commit(
            turn_control,
            turn_control_resolver,
            &TurnTerminal::Committed {
                outcome: delivery.turn.outcome.clone(),
                cancellation: delivery.turn.cancellation.clone(),
                session_revision: None,
            },
            &self.state.session_id,
            &trace_turn_id,
        )
        .await;
        if matches!(delivery.turn.outcome, TurnOutcome::AgentFrameSwitch { .. })
            && let Some(session) = self.session.as_mut()
        {
            let protocol_session = Arc::clone(session.plugins().protocol_session());
            let session_id = self.state.session_id.clone();
            if let Err(err) = protocol_session
                .restore_session(
                    crate::plugin::ProtocolSessionContext::new(session, &session_id),
                    &self.state,
                )
                .await
            {
                delivery.turn.errors.push(post_commit_delivery_issue(
                    "protocol_restore_session",
                    err.to_string(),
                ));
                delivery.post_commit_delivery_failed = true;
                self.invalidate_resident_session_state();
            }
        }
        if !queued_work_completion_trace.is_empty() {
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                lash_trace::TraceContext::default()
                    .for_session(delivery.turn.state.session_id.clone())
                    .for_turn_index(delivery.turn.state.turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::Custom {
                    name: "queued_work.completed".to_string(),
                    payload: queued_work_completion_trace_payload(&queued_work_completion_trace),
                },
                self.host.core.clock.as_ref(),
            );
        }
        if !turn_input_completion_trace.is_empty() {
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                lash_trace::TraceContext::default()
                    .for_session(delivery.turn.state.session_id.clone())
                    .for_turn_index(delivery.turn.state.turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::Custom {
                    name: "turn_input.completed".to_string(),
                    payload: turn_input_completion_trace_payload(&turn_input_completion_trace),
                },
                self.host.core.clock.as_ref(),
            );
        }
        // A final physical turn has already atomically released its lane, so
        // TurnPersisted observers are genuinely lane-less. Agent-frame
        // switches retain the guard and their observers must borrow it.
        let post_commit_session_execution_lease = if release_session_execution_lease {
            None
        } else {
            session_execution_lease
        };
        match self
            .emit_turn_persisted_event(
                &delivery.turn,
                scoped_effect_controller,
                &trace_turn_id,
                post_commit_session_execution_lease,
            )
            .await
        {
            Ok(Some(error)) => {
                let mut issue = crate::plugin::plugin_lifecycle_hook_issue(error);
                issue.retryable = Some(false);
                delivery.turn.errors.push(issue);
                delivery.post_commit_delivery_failed = true;
                self.invalidate_resident_session_state();
            }
            Ok(None) => {}
            Err(err) => {
                delivery
                    .turn
                    .errors
                    .push(post_commit_delivery_issue(err.code.as_str(), err.message));
                delivery.post_commit_delivery_failed = true;
                self.invalidate_resident_session_state();
            }
        }
        self.mark_phase_end(PostCommitDelivery::RUNTIME_PHASE);

        self.emit_completed_turn_trace(
            &delivery.turn.state,
            &delivery.turn.outcome,
            &trace_turn_id,
        );
        Ok(PhysicalTurnExecution {
            turn: delivery.turn,
            enqueued_queue_batches: delivery.enqueued_queue_batches,
            post_commit_delivery_failed: delivery.post_commit_delivery_failed,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_cancelled_turn_after_effect_abort(
        &mut self,
        driver: RuntimeTurnDriver<'_>,
        mut assembler: TurnAssembler,
        cancellation_messages: crate::MessageSequence,
        events: &dyn EventSink,
        finish_scoped_effect_controller: &ScopedEffectController<'_>,
        cancel: &CancellationToken,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        session_execution_lease_release_policy: SessionExecutionLeaseReleasePolicy,
        turn_control: &ActiveTurnControl,
        turn_index: usize,
        trace_turn_id: String,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let RuntimeTurnDriver {
            session,
            policy,
            turn_pipeline,
            pending_queue_claims,
            pending_turn_input_claims,
            ..
        } = driver;
        self.session = Some(session);
        emit_terminal_sequence(&mut assembler, events, None, TurnStop::Cancelled).await;
        let claims = LogicalTurnClaims::new(pending_queue_claims, pending_turn_input_claims);
        Box::pin(self.finish_turn(
            TurnFinishInput {
                turn_pipeline,
                assembler,
                new_messages: cancellation_messages,
                policy,
                turn_index,
                trace_turn_id,
            },
            &claims,
            events,
            finish_scoped_effect_controller,
            cancel,
            session_execution_lease,
            session_execution_lease_release_policy,
            turn_control,
        ))
        .await
    }

    fn emit_completed_turn_trace(
        &self,
        state: &SessionSnapshot,
        outcome: &TurnOutcome,
        trace_turn_id: &str,
    ) {
        if self.host.core.tracing.trace_sink.is_none() {
            return;
        }

        let (status, done_reason, agent_frame_switch) = trace_fields_from_outcome(outcome);
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            lash_trace::TraceContext::default()
                .for_session(state.session_id.clone())
                .for_turn_index(state.turn_index)
                .for_turn(trace_turn_id.to_string()),
            lash_trace::TraceEvent::TurnCompleted {
                status: status.to_string(),
                done_reason: done_reason.to_string(),
                agent_frame_switch,
            },
            self.host.core.clock.as_ref(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish_logical_turn_error(
        &mut self,
        message: String,
        trace_turn_id: String,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        cancel: CancellationToken,
        claims: LogicalTurnClaims,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_resolver =
            turn_control_resolver(turn_control_host.as_ref(), &scoped_effect_controller);
        let turn_control = Arc::new(
            ActiveTurnControl::new(
                turn_control_resolver,
                TurnAddress::new(&self.state.session_id, &trace_turn_id),
            )
            .await?,
        );
        let mut assembler = TurnAssembler::default();
        emit_terminal_sequence(
            &mut assembler,
            events,
            Some(TerminalDiagnostic {
                kind: TerminalDiagnosticKind::Runtime,
                code: Some("agent_frame_switch_limit".to_string()),
                message,
                retryable: Some(false),
                activity: TerminalActivityTarget::UnscopedSink {
                    sink: turn_events,
                    turn_id: &trace_turn_id,
                },
            }),
            TurnStop::RuntimeError,
        )
        .await;

        let messages = crate::MessageSequence::from_base(self.state.read_model().messages);
        let mut turn_pipeline = TurnBoundary::from_state_with_clock(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
        );
        turn_pipeline.apply_prepared_messages(&messages);
        let finish_result = Box::pin(self.finish_turn(
            TurnFinishInput {
                turn_pipeline,
                assembler,
                new_messages: messages,
                policy: RuntimeSessionPolicy::new(
                    self.state.effective_policy().clone(),
                    Default::default(),
                ),
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                turn_index: self.state.turn_index + 1,
                trace_turn_id,
            },
            &claims,
            events,
            &scoped_effect_controller,
            &cancel,
            session_execution_lease,
            SessionExecutionLeaseReleasePolicy::KeepOnAgentFrameSwitch,
            &turn_control,
        ))
        .await;
        if let Err(err) = &finish_result {
            self.abandon_queued_work_claims_after_local_abort(err, &claims.queued)
                .await;
            self.abandon_turn_input_claims_after_local_abort(err, &claims.turn_inputs)
                .await;
        }
        finish_result
    }

    async fn emit_turn_persisted_event(
        &self,
        returned_turn: &AssembledTurn,
        scoped_effect_controller: &ScopedEffectController<'_>,
        trace_turn_id: &str,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<Option<crate::PluginError>, RuntimeError> {
        let Some(session) = self.session.as_ref() else {
            return Ok(None);
        };
        let manager = self
            .runtime_session_services_after_commit(session_execution_lease)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let phase_turn_id = turn_phase_id(trace_turn_id, "turn-persisted");
        let phase_controller = scoped_child_turn_controller(
            scoped_effect_controller,
            &self.state.session_id,
            &phase_turn_id,
        )?;
        let direct_completions = manager.direct_completion_client(
            RuntimeEffectControllerHandle::borrowed(phase_controller),
            Some(phase_turn_id),
        );

        let result = session
            .plugins()
            .emit_runtime_event_with_phase_probe(
                crate::PluginLifecycleEvent::TurnPersisted(Box::new(
                    crate::SessionStateChangedContext {
                        session_id: self.state.session_id.clone(),
                        state: crate::SessionReadView::from_snapshot(&returned_turn.state),
                        sessions: manager.state_service(),
                        session_graph: manager.graph_service(),
                        direct_completions,
                    },
                )),
                self.turn_phase_probe.clone(),
            )
            .await;
        Ok(result.err())
    }

    /// Run one logical turn and stream every physical frame to the host sink.
    pub async fn stream_turn(
        &mut self,
        mut input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        if let Some(hint) = opts.local_cancel_origin_hint() {
            input.turn_context.set_local_cancel_origin_hint(hint);
        }
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let cancel = opts.cancel.clone();
        let mut session_execution_lease = self.claim_session_execution_lease().await?;
        let scoped_effect_controller = opts.scoped_effect_controller();
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Input(input),
            opts.events_or_noop(),
            opts.turn_events_or_noop(),
            scoped_effect_controller,
            cancel,
            LogicalTurnClaims::new(Vec::new(), Vec::new()),
            &mut session_execution_lease,
            stopwatch,
        ))
        .await
        .map(|run| {
            run.into_final_turn()
                .expect("logical turn always contains a terminal physical turn")
        });
        self.settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await
    }

    pub async fn stream_next_queued_work(
        &mut self,
        opts: TurnOptions<'_>,
    ) -> Result<Option<AssembledTurn>, RuntimeError> {
        self.stream_queued_work(opts, None).await
    }

    pub async fn stream_selected_queued_work(
        &mut self,
        opts: TurnOptions<'_>,
        batch_ids: &[String],
    ) -> Result<Option<AssembledTurn>, RuntimeError> {
        self.stream_queued_work(opts, Some(batch_ids)).await
    }

    async fn stream_queued_work(
        &mut self,
        opts: TurnOptions<'_>,
        selected_batch_ids: Option<&[String]>,
    ) -> Result<Option<AssembledTurn>, RuntimeError> {
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let cancel = opts.cancel.clone();
        let Some(session_execution_lease) = self.claim_session_execution_lease().await? else {
            return Ok(None);
        };
        // This snapshot stays current while leading commands drain because
        // `RefreshToolCatalog` never acquires a fresh session lease; any later
        // lease rotation happens only after this command-drain window.
        let session_execution_fence = session_execution_lease.fence();
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            session_execution_lease
                .release_if_live()
                .await
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
                })?;
            return Ok(None);
        };
        let drain_commands_before_turn_input = if selected_batch_ids.is_some() {
            true
        } else {
            self.session_commands_precede_pending_turn_input(store.as_ref())
                .await?
        };
        if drain_commands_before_turn_input {
            loop {
                match self
                    .drain_next_session_command(&session_execution_fence)
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(err) => {
                        let _ = session_execution_lease.release_if_live().await;
                        return Err(err);
                    }
                }
            }
        }
        if selected_batch_ids.is_none() {
            let input_claim = store
                .claim_next_turn_inputs(
                    &self.state.session_id,
                    &session_execution_fence,
                    &self.runtime_lease_owner,
                    64,
                )
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            if let Some(input_claim) = input_claim {
                let mut input = input_claim.materialize_turn_input();
                if let Some(hint) = opts.local_cancel_origin_hint() {
                    input.turn_context.set_local_cancel_origin_hint(hint);
                }
                let turn_id = input
                    .trace_turn_id
                    .clone()
                    .unwrap_or_else(|| opts.execution_scope_id().to_owned());
                input.trace_turn_id = Some(turn_id.clone());
                crate::trace::emit_trace(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    lash_trace::TraceContext::default()
                        .for_session(self.state.session_id.clone())
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        .for_turn_index(self.state.turn_index + 1)
                        .for_turn(turn_id.clone()),
                    lash_trace::TraceEvent::Custom {
                        name: "turn_input.claimed".to_string(),
                        payload: serde_json::json!({
                            "claim_id": &input_claim.claim_id,
                            "input_ids": input_claim.inputs.iter().map(|input| input.input_id.clone()).collect::<Vec<_>>(),
                        }),
                    },
                    self.host.core.clock.as_ref(),
                );
                let claim_for_abandon = input_claim.clone();
                let scoped_effect_controller = opts.scoped_effect_controller();
                let mut session_execution_lease = Some(session_execution_lease);
                let result = Box::pin(self.drive_logical_turn(
                    LogicalTurnStart::Input(input),
                    opts.events_or_noop(),
                    opts.turn_events_or_noop(),
                    scoped_effect_controller,
                    cancel,
                    LogicalTurnClaims::new(Vec::new(), vec![input_claim]),
                    &mut session_execution_lease,
                    stopwatch,
                ))
                .await
                .map(AgentFrameRun::into_final_turn);
                if let Err(err) = &result {
                    self.abandon_turn_input_claims_after_local_abort(
                        err,
                        std::slice::from_ref(&claim_for_abandon),
                    )
                    .await;
                }
                return self
                    .settle_session_execution_lease(session_execution_lease.as_ref(), result)
                    .await;
            }
        }
        let claim = if let Some(batch_ids) = selected_batch_ids {
            let claim_policy = self
                .host
                .core
                .durability
                .queued_work_batching
                .claim_policy(self.max_context_tokens());
            store
                .claim_ready_queued_work_by_batch_ids(
                    &self.state.session_id,
                    &session_execution_fence,
                    &self.runtime_lease_owner,
                    crate::QueuedWorkClaimBoundary::Idle,
                    batch_ids,
                    claim_policy,
                )
                .await
        } else {
            let claim_policy = self
                .host
                .core
                .durability
                .queued_work_batching
                .claim_policy(self.max_context_tokens());
            store
                .claim_ready_queued_work(
                    &self.state.session_id,
                    &session_execution_fence,
                    &self.runtime_lease_owner,
                    crate::QueuedWorkClaimBoundary::Idle,
                    claim_policy,
                )
                .await
        }
        .map_err(super::runtime_error_from_store_commit)?;
        let Some(claim) = claim else {
            session_execution_lease
                .release_if_live()
                .await
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
                })?;
            return Ok(None);
        };
        let mut work = claim.materialize_queued_turn_work();
        if selected_batch_ids.is_some() {
            // A host-selected drain is closed over the rendered batch set. Without this guard,
            // an EarliestSafeBoundary checkpoint in the selected turn could pull unrelated
            // pending batches into the same run after the exact initial claim.
            work.input.turn_context.suppress_checkpoint_queued_work();
        }
        if let Some(hint) = opts.local_cancel_origin_hint() {
            work.input.turn_context.set_local_cancel_origin_hint(hint);
        }
        let turn_id = work
            .input
            .trace_turn_id
            .clone()
            .unwrap_or_else(|| opts.execution_scope_id().to_owned());
        work.input.trace_turn_id = Some(turn_id.clone());
        let causes = work.turn_causes.clone();
        emit_queued_work_started_to_sink(
            opts.turn_events_or_noop(),
            &turn_id,
            crate::QueuedWorkClaimBoundary::Idle,
            &claim,
            causes.clone(),
        )
        .await;
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            lash_trace::TraceContext::default()
                .for_session(self.state.session_id.clone())
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                .for_turn_index(self.state.turn_index + 1)
                .for_turn(turn_id.clone()),
            lash_trace::TraceEvent::Custom {
                name: "queued_work.claimed".to_string(),
                payload: queued_work_trace_payload(
                    crate::QueuedWorkClaimBoundary::Idle,
                    &claim,
                    &causes,
                ),
            },
            self.host.core.clock.as_ref(),
        );
        let claim_for_abandon = claim.clone();
        let scoped_effect_controller = opts.scoped_effect_controller();
        let mut session_execution_lease = Some(session_execution_lease);
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Input(work.input),
            opts.events_or_noop(),
            opts.turn_events_or_noop(),
            scoped_effect_controller,
            cancel,
            LogicalTurnClaims::new(vec![claim], Vec::new()),
            &mut session_execution_lease,
            stopwatch,
        ))
        .await
        .map(AgentFrameRun::into_final_turn);
        if let Err(err) = &result {
            self.abandon_queued_work_claims_after_local_abort(
                err,
                std::slice::from_ref(&claim_for_abandon),
            )
            .await;
        }
        self.settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await
    }

    async fn session_commands_precede_pending_turn_input(
        &self,
        store: &dyn crate::RuntimePersistence,
    ) -> Result<bool, RuntimeError> {
        let pending_inputs = store
            .list_pending_turn_inputs(&self.state.session_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let earliest_input = pending_inputs
            .iter()
            .filter(|input| input.state.is_next_turn_pending())
            .min_by_key(|input| (input.enqueued_at_ms, input.enqueue_seq));
        let queued_work = store
            .list_pending_queued_work(&self.state.session_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let earliest_command = queued_work
            .iter()
            .filter(|batch| batch.is_session_command_work())
            .min_by_key(|batch| (batch.enqueued_at_ms, batch.enqueue_seq));
        Ok(match (earliest_command, earliest_input) {
            (Some(command), Some(input)) => command.enqueued_at_ms < input.enqueued_at_ms,
            (Some(_), None) => true,
            _ => false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_turn_with_scoped_effect_controller_inner(
        &mut self,
        mut input: TurnInput,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        cancel: CancellationToken,
        queued_claims: Vec<crate::QueuedWorkClaim>,
        turn_input_claims: Vec<crate::TurnInputClaim>,
        materialize_initial_claims: bool,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        session_execution_lease_release_policy: SessionExecutionLeaseReleasePolicy,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        if queued_claims.is_empty()
            && turn_input_claims.is_empty()
            && let Some(lease) = session_execution_lease
        {
            while self
                .drain_next_session_command(&lease.fence())
                .await?
                .is_some()
            {}
        }
        if let Some(input_turn_id) = input.trace_turn_id.as_deref()
            && scoped_effect_controller
                .execution_scope()
                .validates_turn_trace_id()
            && input_turn_id != scoped_effect_controller.scope_id()
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "input trace_turn_id `{input_turn_id}` does not match execution scope id `{}`",
                    scoped_effect_controller.scope_id()
                ),
            ));
        }
        let turn_id = input
            .trace_turn_id
            .get_or_insert_with(|| scoped_effect_controller.scope_id().to_string())
            .clone();
        let scoped_effect_controller =
            scoped_effect_controller.rescope(self.state.turn_scope(&turn_id))?;
        // The stable execution-scope turn id is attached to every write-ahead
        // intent before ingress, tools, plugins, or envelope normalization can
        // put bytes. Replays bind the same id; no live pending-id state is used.
        let _attachment_owner_binding = self
            .host
            .core
            .durability
            .attachment_store
            .bind_turn_scoped(turn_id);
        Box::pin(self.stream_turn_inner(
            input.clone(),
            events,
            turn_events,
            scoped_effect_controller,
            cancel.clone(),
            queued_claims,
            turn_input_claims,
            materialize_initial_claims,
            session_execution_lease,
            session_execution_lease_release_policy,
        ))
        .await
    }

    /// Stream one logical host turn, following foreground AgentFrame switches
    /// until a terminal outcome is reached.
    ///
    /// A protocol continuation creates a new frame in the same session. Hosts
    /// that only care about the benchmark/app answer should not need to
    /// special-case that intermediate outcome; this helper keeps driving the
    /// same session through each frame's task with the normal runtime turn
    /// guards.
    pub async fn stream_turn_with_agent_frames(
        &mut self,
        mut input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        if let Some(hint) = opts.local_cancel_origin_hint() {
            input.turn_context.set_local_cancel_origin_hint(hint);
        }
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let cancel = opts.cancel.clone();
        let mut session_execution_lease = self.claim_session_execution_lease().await?;
        let scoped_effect_controller = opts.scoped_effect_controller();
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Input(input),
            opts.events_or_noop(),
            opts.turn_events_or_noop(),
            scoped_effect_controller,
            cancel,
            LogicalTurnClaims::new(Vec::new(), Vec::new()),
            &mut session_execution_lease,
            stopwatch,
        ))
        .await;
        self.settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_turn_inner(
        &mut self,
        mut input: TurnInput,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        cancel: CancellationToken,
        queued_claims: Vec<crate::QueuedWorkClaim>,
        mut turn_input_claims: Vec<crate::TurnInputClaim>,
        materialize_initial_claims: bool,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        session_execution_lease_release_policy: SessionExecutionLeaseReleasePolicy,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        let lease_continuity =
            session_execution_lease.and_then(SessionExecutionLeaseGuard::continuity);
        let resident_graph_is_current = self.graph_loaded_from_store
            && !self.resident_graph_head_stale.load(Ordering::Acquire)
            && lease_continuity.is_some()
            && lease_continuity == self.last_committed_lease_continuity;
        if !resident_graph_is_current {
            self.refresh_session_graph_from_store()
                .await
                .map_err(session_head_refresh_error)?;
        }
        // `load_session` refreshes the committed graph/head, checkpoint,
        // config, frames, and token ledger. It does not cover pending turn
        // inputs, queued work, or trigger deliveries; those remain external
        // ingress and are picked up by their fenced claim paths.
        let input_trace_turn_id = input.trace_turn_id.clone();
        let queued_turn_work = materialize_initial_claims
            .then(|| queued_claims.first())
            .flatten()
            .map(crate::QueuedWorkClaim::materialize_queued_turn_work);
        let pending_turn_input = materialize_initial_claims
            .then(|| turn_input_claims.first())
            .flatten()
            .map(crate::TurnInputClaim::materialize_turn_input);
        if let Some(work) = pending_turn_input.as_ref()
            && input.items.is_empty()
        {
            let turn_context = input.turn_context.clone();
            input = work.clone();
            // Retain host controls installed on the initially materialized input. The claim is
            // rematerialized here to refresh durable payloads, not to erase live run policy.
            input.turn_context = turn_context;
            if input.trace_turn_id.is_none() {
                input.trace_turn_id = input_trace_turn_id.clone();
            }
        }
        if let Some(work) = queued_turn_work.as_ref()
            && input.items.is_empty()
        {
            let turn_context = input.turn_context.clone();
            input = work.input.clone();
            input.turn_context = turn_context;
            if input.trace_turn_id.is_none() {
                input.trace_turn_id = input_trace_turn_id;
            }
        }
        if self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .is_some()
        {
            ensure_durable_effect_input(&input)?;
        }
        if let Some(extension) = &input.protocol_extension
            && let Some(session) = self.session.as_ref()
        {
            let protocol_session = std::sync::Arc::clone(session.plugins().protocol_session());
            protocol_session
                .validate_turn_extension(extension)
                .await
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::ProtocolTurnExtension, err.to_string())
                })?;
        }
        let previous_prompt_usage = self.state.last_prompt_usage.clone();
        let normalized = match self.normalize_input_items(&input.items).await {
            Ok(items) => items,
            Err(e) => {
                self.state.last_prompt_usage = None;
                let mut assembler = TurnAssembler::default();
                let trace_turn_id = input
                    .trace_turn_id
                    .clone()
                    .expect("turn id is bound from the execution scope before validation");
                emit_terminal_sequence(
                    &mut assembler,
                    events,
                    Some(TerminalDiagnostic {
                        kind: TerminalDiagnosticKind::InputValidation,
                        code: Some("invalid_turn_input".to_string()),
                        message: e,
                        retryable: Some(false),
                        activity: TerminalActivityTarget::UnscopedSink {
                            sink: turn_events,
                            turn_id: &trace_turn_id,
                        },
                    }),
                    TurnStop::InvalidInput,
                )
                .await;
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                let turn_index = self.state.turn_index + 1;
                let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
                let turn_control_resolver =
                    turn_control_resolver(turn_control_host.as_ref(), &scoped_effect_controller);
                let turn_control = ActiveTurnControl::new(
                    turn_control_resolver,
                    TurnAddress::new(&self.state.session_id, &trace_turn_id),
                )
                .await?
                .with_local_cancel_origin(input.turn_context.local_cancel_origin_hint());
                let messages = crate::MessageSequence::from_base(self.state.read_model().messages);
                let mut turn_pipeline = TurnBoundary::from_state_with_clock(
                    self.state.clone(),
                    Arc::clone(&self.host.core.clock),
                    self.state.turn_scope(&trace_turn_id),
                    self.host.core.durability.commit_budget,
                );
                turn_pipeline.apply_prepared_messages(&messages);
                let claims = LogicalTurnClaims::new(queued_claims, turn_input_claims);
                return Box::pin(self.finish_turn(
                    TurnFinishInput {
                        turn_pipeline,
                        assembler,
                        new_messages: messages,
                        policy: RuntimeSessionPolicy::new(
                            self.state.effective_policy().clone(),
                            Default::default(),
                        ),
                        turn_index,
                        trace_turn_id,
                    },
                    &claims,
                    events,
                    &scoped_effect_controller,
                    &cancel,
                    session_execution_lease,
                    session_execution_lease_release_policy,
                    &turn_control,
                ))
                .await;
            }
        };
        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
        let turn_index = self.state.turn_index + 1;
        let trace_turn_id = input
            .trace_turn_id
            .clone()
            .expect("turn id is bound from the execution scope before normalization");
        if self.host.core.tracing.trace_sink.is_some() {
            let mut trace_metadata = std::collections::BTreeMap::new();
            trace_metadata.insert(
                "input_item_count".to_string(),
                serde_json::json!(normalized.len()),
            );
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                lash_trace::TraceContext::default()
                    .for_session(self.state.session_id.clone())
                    .for_turn_index(turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::TurnStarted {
                    metadata: trace_metadata,
                },
                self.host.core.clock.as_ref(),
            );
        }

        let base_read_model = self.state.read_model();
        let base_messages = base_read_model.messages;
        let base_render_cache = base_read_model.prompt_render_cache;
        let mut turn_delta = Vec::new();
        let initial_turn_causes = queued_turn_work
            .as_ref()
            .map(|work| work.turn_causes.clone())
            .unwrap_or_default();
        turn_delta.extend(
            initial_turn_causes
                .iter()
                .map(crate::TurnCause::to_event_message),
        );

        let turn_input_id = turn_input_claims
            .iter()
            .flat_map(|claim| claim.inputs.iter().map(|input| input.input_id.clone()))
            .next();
        let user_id = turn_input_id
            .as_deref()
            .map(crate::runtime::ingress_message_id)
            .unwrap_or_else(|| format!("m_turn_{trace_turn_id}_input"));
        let mut user_parts: Vec<Part> = Vec::new();
        for item in normalized {
            match item {
                NormalizedItem::Text(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    user_parts.push(Part::text(
                        format!("{}.p{}", user_id, user_parts.len()),
                        text,
                        None,
                    ));
                }
                NormalizedItem::Attachment(source) => {
                    user_parts.push(Part::attachment_part(
                        format!("{}.p{}", user_id, user_parts.len()),
                        String::new(),
                        Some(crate::session_model::message::PartAttachment { source }),
                    ));
                }
            }
        }
        if user_parts.is_empty() && initial_turn_causes.is_empty() {
            user_parts.push(Part::text(format!("{}.p0", user_id), String::new(), None));
        }
        if !user_parts.is_empty() {
            reassign_part_ids(&user_id, &mut user_parts);
            turn_delta.push(Message {
                id: user_id.clone(),
                role: MessageRole::User,
                parts: shared_parts(user_parts),
                // Typed provenance, not a pinned id: a host that rendered its
                // own row for this turn recognizes the committed copy by
                // `turn_id` (FIG-972).
                origin: Some(crate::MessageOrigin::TurnInput {
                    turn_id: trace_turn_id.clone(),
                    input_id: turn_input_id.clone(),
                }),
            });
        }
        let mut initial_turn_input_applications = Vec::new();
        for claim in &mut turn_input_claims {
            claim.record_initial_turn_application(&crate::TurnId::from(&trace_turn_id), &user_id);
            initial_turn_input_applications.extend(claim.applications.clone());
        }
        if !initial_turn_input_applications.is_empty() {
            emit_turn_activity_to_sink_for_turn(
                turn_events,
                &trace_turn_id,
                TurnActivity::independent(TurnEvent::QueuedInputAccepted {
                    applications: initial_turn_input_applications,
                }),
            )
            .await;
        }

        let manager = self
            .runtime_session_services_for_turn(None, session_execution_lease)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let plugin_session = self
            .session
            .as_ref()
            .map(|s| Arc::clone(s.plugins()))
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::ContextPrepareTurn,
                    "runtime session not available",
                )
            })?;
        let prepare_phase_turn_id = turn_phase_id(&trace_turn_id, "prepare-turn");
        let prepare_phase_controller = scoped_child_turn_controller(
            &scoped_effect_controller,
            &self.state.session_id,
            &prepare_phase_turn_id,
        )?;
        let turn_ctx = crate::TurnTransformContext {
            session_id: self.state.session_id.clone(),
            state: self.read_view(),
            prompt_usage: previous_prompt_usage.clone(),
            max_context_tokens: Some(LashRuntime::max_context_tokens(self)),
            sessions: manager.state_service(),
            session_lifecycle: manager.lifecycle_service(),
            session_graph: manager.graph_service(),
            scoped_effect_controller: scoped_effect_controller.clone(),
            direct_completions: manager.direct_completion_client(
                RuntimeEffectControllerHandle::borrowed(prepare_phase_controller),
                Some(prepare_phase_turn_id),
            ),
        };
        self.mark_phase_begin(RuntimeTurnPhase::ContextTransform);
        let prepared_context = plugin_session
            .prepare_turn_context(
                &turn_ctx,
                crate::session_model::context::PreparedContext {
                    messages: crate::MessageSequence::from_base_and_delta(
                        base_messages,
                        turn_delta,
                    )
                    .with_base_render_cache(base_render_cache),
                    ..Default::default()
                },
                self.turn_phase_probe.clone(),
            )
            .await
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, err.to_string())
            })?;
        self.mark_phase_end(RuntimeTurnPhase::ContextTransform);
        // Release the read-view's graph clone before the rest of the turn
        // runs. Keeping it alive into `stream_prepared_turn` forces the
        // post-turn `append_active_read_delta` to deep-clone the session
        // graph (Arc::make_mut with refcount > 1).
        drop(turn_ctx);
        let messages = prepared_context.messages;
        if let Some(session) = self.session.as_mut() {
            session
                .set_context_overlay(
                    prepared_context.tool_providers,
                    prepared_context.prompt_contributions,
                    prepared_context.include_base_tools,
                )
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::SessionToolRegistry, err.to_string())
                })?;
        }

        self.state.last_prompt_usage = None;
        Box::pin(self.stream_prepared_turn_inner(
            messages,
            previous_prompt_usage,
            input.protocol_turn_options.clone(),
            input.protocol_extension.clone(),
            input.turn_context.clone(),
            initial_turn_causes,
            trace_turn_id,
            turn_index,
            events,
            turn_events,
            scoped_effect_controller,
            cancel,
            queued_claims,
            turn_input_claims,
            session_execution_lease,
            session_execution_lease_release_policy,
        ))
        .await
    }

    /// Run one logical turn and return only its assembled terminal result.
    pub async fn run_turn_assembled(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        self.stream_turn(input, TurnOptions::new(cancel, scoped_effect_controller))
            .await
    }

    /// Run one logical turn using host-prepared message history.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_prepared_turn(
        &mut self,
        messages: crate::MessageSequence,
        previous_prompt_usage: Option<PromptUsage>,
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
        protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
        turn_context: crate::TurnContext,
        initial_turn_causes: Vec<crate::TurnCause>,
        trace_turn_id: String,
        turn_index: usize,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        cancel: CancellationToken,
        initial_queue_claim: Option<crate::QueuedWorkClaim>,
        initial_turn_input_claim: Option<crate::TurnInputClaim>,
    ) -> Result<AssembledTurn, RuntimeError> {
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let mut session_execution_lease = self.claim_session_execution_lease().await?;
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Prepared(PreparedLogicalTurn {
                messages,
                previous_prompt_usage,
                protocol_turn_options,
                protocol_extension,
                turn_context,
                initial_turn_causes,
                trace_turn_id,
                turn_index,
            }),
            events,
            turn_events,
            scoped_effect_controller,
            cancel,
            LogicalTurnClaims::new(
                initial_queue_claim.into_iter().collect(),
                initial_turn_input_claim.into_iter().collect(),
            ),
            &mut session_execution_lease,
            stopwatch,
        ))
        .await
        .map(|run| {
            run.into_final_turn()
                .expect("logical turn always contains a terminal physical turn")
        });
        self.settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_turn_preamble(
        &mut self,
        plugins: &crate::PluginSession,
        manager: &Arc<RuntimeSessionServices>,
        messages: crate::MessageSequence,
        turn_policy: &crate::SessionPolicy,
        effective_protocol_turn_options: &crate::ProtocolTurnOptions,
        turn_context: &crate::TurnContext,
        turn_scope_id: &str,
        event_rx: &mut mpsc::Receiver<RuntimeStreamEvent>,
        assembler: &mut TurnAssembler,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
    ) -> Result<crate::plugin::TurnPreparation, RuntimeError> {
        self.mark_phase_begin(RuntimeTurnPhase::BeforeTurnHooks);
        let prepare_turn = plugins.prepare_turn_with_phase_probe(
            PrepareTurnRequest {
                session_id: self.state.session_id.clone(),
                state: crate::SessionReadView::from_runtime_state(
                    &self.state,
                    turn_policy.clone(),
                    effective_protocol_turn_options.clone(),
                ),
                messages,
                sessions: manager.state_service(),
                session_lifecycle: manager.lifecycle_service(),
                session_graph: manager.graph_service(),
                turn_context: turn_context.clone(),
            },
            self.turn_phase_probe.clone(),
            turn_scope_id,
        );
        let mut prepare_turn = Box::pin(prepare_turn);

        let mut event_pump = RuntimeStreamEventPump {
            assembler,
            events,
            turn_events,
        };
        let prepared = drive_with_event_pump(
            prepare_turn.as_mut(),
            event_rx,
            &mut event_pump,
            |pump, event| {
                Box::pin(async move {
                    pump.emit(event).await;
                })
            },
        )
        .await
        .map_err(|err| RuntimeError::new(RuntimeErrorCode::PluginPrepareTurn, err.to_string()))?;
        self.mark_phase_end(RuntimeTurnPhase::BeforeTurnHooks);
        Ok(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_prepared_turn_abort(
        &mut self,
        prepared: crate::plugin::TurnPreparation,
        event_tx: mpsc::Sender<RuntimeStreamEvent>,
        mut assembler: TurnAssembler,
        turn_index: usize,
        trace_turn_id: String,
        claims: &LogicalTurnClaims,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: &ScopedEffectController<'_>,
        cancel: &CancellationToken,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        session_execution_lease_release_policy: SessionExecutionLeaseReleasePolicy,
        _session_execution_fence: Option<crate::SessionExecutionLeaseAuthority>,
        turn_control: &ActiveTurnControl,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let Some(abort) = prepared.abort else {
            unreachable!("abort finisher requires a prepared plugin abort");
        };
        drop(event_tx);

        // The preparation future and its SessionReadView are gone before this
        // state clone. That keeps the graph from being held twice while the
        // turn boundary takes ownership of its working state.
        let mut turn_pipeline = TurnBoundary::from_state_with_clock(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
        );
        turn_pipeline.apply_prepared_messages(&prepared.messages);
        emit_terminal_sequence(
            &mut assembler,
            events,
            Some(TerminalDiagnostic {
                kind: TerminalDiagnosticKind::Plugin,
                code: Some(abort.code),
                message: abort.message,
                retryable: None,
                activity: TerminalActivityTarget::TurnScopedSink(turn_events),
            }),
            TurnStop::PluginAbort,
        )
        .await;
        Box::pin(self.finish_turn(
            TurnFinishInput {
                turn_pipeline,
                assembler,
                new_messages: prepared.messages,
                policy: RuntimeSessionPolicy::new(
                    self.state.effective_policy().clone(),
                    Default::default(),
                ),
                turn_index,
                trace_turn_id,
            },
            claims,
            events,
            scoped_effect_controller,
            cancel,
            session_execution_lease,
            session_execution_lease_release_policy,
            turn_control,
        ))
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn stream_prepared_turn_inner(
        &mut self,
        messages: crate::MessageSequence,
        _previous_prompt_usage: Option<PromptUsage>,
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
        protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
        turn_context: crate::TurnContext,
        initial_turn_causes: Vec<crate::TurnCause>,
        trace_turn_id: String,
        turn_index: usize,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        cancel: CancellationToken,
        initial_queue_claims: Vec<crate::QueuedWorkClaim>,
        initial_turn_input_claims: Vec<crate::TurnInputClaim>,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        session_execution_lease_release_policy: SessionExecutionLeaseReleasePolicy,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let scoped_turn_events = TurnScopedActivitySink {
            turn_id: trace_turn_id.clone(),
            inner: turn_events,
        };
        let turn_events: &dyn TurnActivitySink = &scoped_turn_events;
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_resolver =
            turn_control_resolver(turn_control_host.as_ref(), &scoped_effect_controller);
        let inline_turn_control_controller =
            if turn_control_host.replay_ownership() == crate::EffectReplayOwnership::Runtime {
                Some(turn_control_host.scoped(scoped_effect_controller.execution_scope().clone())?)
            } else {
                None
            };
        let turn_control = Arc::new(
            ActiveTurnControl::new(
                turn_control_resolver,
                TurnAddress::new(&self.state.session_id, &trace_turn_id),
            )
            .await?
            .with_local_cancel_origin(turn_context.local_cancel_origin_hint()),
        );
        let session_execution_fence =
            session_execution_lease.map(SessionExecutionLeaseGuard::fence);
        let (event_tx, mut event_rx) = mpsc::channel::<RuntimeStreamEvent>(100);
        let child_usage_event_relay = ChildUsageEventRelay::new(event_tx.clone());
        let mut turn_policy = self.state.effective_policy().clone();
        let turn_provider_override = turn_context.provider().cloned();
        if let Some(provider) = turn_provider_override.as_ref() {
            turn_policy.provider_id = provider.kind().to_string();
        }
        let session_protocol_turn_options = self.state.effective_protocol_turn_options().clone();
        let effective_protocol_turn_options = protocol_turn_options
            .clone()
            .map(|options| session_protocol_turn_options.merged_with_override(&options))
            .unwrap_or(session_protocol_turn_options);
        let manager = self
            .runtime_session_services_for_turn(
                Some(child_usage_event_relay.clone()),
                session_execution_lease,
            )
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let plugins = {
            let session = self
                .session
                .as_ref()
                .expect("lash runtime session must be available");
            Arc::clone(session.plugins())
        };
        let mut assembler = TurnAssembler::new();
        let initial_claims =
            LogicalTurnClaims::new(initial_queue_claims, initial_turn_input_claims);
        // Keep preparation and plugin-abort handling in separate async frames.
        // Their SessionReadView and abort-only locals are dropped before the
        // normal driver-construction frame clones state for the turn boundary.
        let mut prepared = self
            .prepare_turn_preamble(
                plugins.as_ref(),
                &manager,
                messages,
                &turn_policy,
                &effective_protocol_turn_options,
                &turn_context,
                &trace_turn_id,
                &mut event_rx,
                &mut assembler,
                events,
                turn_events,
            )
            .await?;
        for event in &prepared.events {
            assembler.push(event);
        }
        emit_session_events_to_sink(events, std::mem::take(&mut prepared.events)).await;
        if prepared.abort.is_some() {
            return Box::pin(self.finish_prepared_turn_abort(
                prepared,
                event_tx,
                assembler,
                turn_index,
                trace_turn_id,
                &initial_claims,
                events,
                turn_events,
                &scoped_effect_controller,
                &cancel,
                session_execution_lease,
                session_execution_lease_release_policy,
                session_execution_fence,
                turn_control.as_ref(),
            ))
            .await;
        }
        // `prepare_turn_preamble` has returned and dropped its read-view frame
        // before this clone, avoiding a transient second graph owner.
        let mut turn_pipeline = TurnBoundary::from_state_with_clock(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
        );
        turn_pipeline
            .prepared_checkpoint(
                turn_policy.clone(),
                turn_index,
                &prepared.messages,
                self.session.as_mut(),
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let resolved_turn_policy = if let Some(provider) = turn_provider_override {
            RuntimeSessionPolicy::from_provider(
                turn_policy.clone(),
                provider.with_clock(Arc::clone(&self.host.core.clock)),
            )
            .map_err(|err| {
                RuntimeError::new(crate::RuntimeErrorCode::LlmProvider, err.to_string())
            })?
        } else {
            self.host
                .resolve_session_policy(&self.state.session_id, turn_policy.clone())
                .map_err(|err| {
                    RuntimeError::new(crate::RuntimeErrorCode::LlmProvider, err.to_string())
                })?
        };
        let manager = self
            .runtime_session_services_for_turn(
                Some(child_usage_event_relay.clone()),
                session_execution_lease,
            )
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let cancel_state = cancel.clone();
        let finish_scoped_effect_controller = scoped_effect_controller.clone();
        let turn_cancel_peek_controller = inline_turn_control_controller
            .as_ref()
            .map(ScopedEffectController::controller)
            .unwrap_or_else(|| finish_scoped_effect_controller.controller());
        let observes_durable_cancel_after_llm = turn_control_host.replay_ownership()
            == crate::EffectReplayOwnership::Controller
            && finish_scoped_effect_controller
                .controller()
                .replay_ownership()
                == crate::EffectReplayOwnership::Controller;
        let session = self
            .session
            .take()
            .expect("lash runtime session must be available");
        let driver = Box::new(RuntimeTurnDriver {
            session,
            policy: resolved_turn_policy,
            host: self.host.clone(),
            turn_id: crate::TurnId::from(scoped_effect_controller.scope_id()),
            scoped_effect_controller,
            session_id: self.state.session_id.clone(),
            turn_index,
            turn_pipeline,
            llm_stream_summaries: HashMap::new(),
            llm_calls: Vec::new(),
            next_llm_ordinal: 0,
            session_services: manager,
            protocol_turn_options: effective_protocol_turn_options,
            protocol_extension,
            turn_context,
            turn_causes: initial_turn_causes,
            pending_queue_claims: initial_claims.queued,
            pending_turn_input_claims: initial_claims.turn_inputs,
            pending_checkpoint_turn_input_claim: None,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            session_execution_lease: session_execution_fence,
            runtime_lease_owner: self.runtime_lease_owner.clone(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            turn_control: Arc::clone(&turn_control),
            observes_durable_cancel_after_llm,
        });
        let protocol_run_offset = 0;
        self.mark_phase_begin(RuntimeTurnPhase::EffectLoop);
        let mut driver = TurnDriverSessionLoan::new(&mut self.session, driver);
        let run_result = Box::pin(run_turn_effect_loop(
            &mut driver,
            prepared.messages,
            event_tx,
            cancel.clone(),
            protocol_run_offset,
            Arc::clone(&turn_control),
            Arc::clone(&turn_control_host),
            turn_cancel_peek_controller,
            &mut event_rx,
            &mut assembler,
            &child_usage_event_relay,
            events,
            turn_events,
        ))
        .await;
        let (new_messages, _new_protocol_iteration) = match run_result {
            Ok(result) => result,
            Err(err) if cancel.is_cancelled() => {
                if turn_control.evidence().is_none() {
                    turn_control
                        .observe_pending_cancel(
                            turn_cancel_peek_controller,
                            crate::runtime::turn_control::TurnCancelPeekIdentity::PostAbortGate,
                        )
                        .await?;
                }
                if turn_control.evidence().is_some() {
                    let driver = driver.into_inner();
                    self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                    let cancellation_messages = driver.turn_pipeline.message_sequence();
                    return Box::pin(self.finish_cancelled_turn_after_effect_abort(
                        *driver,
                        assembler,
                        cancellation_messages,
                        events,
                        &finish_scoped_effect_controller,
                        &cancel,
                        session_execution_lease,
                        session_execution_lease_release_policy,
                        turn_control.as_ref(),
                        turn_index,
                        trace_turn_id,
                    ))
                    .await;
                }
                let driver = driver.into_inner();
                self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                let RuntimeTurnDriver {
                    session,
                    pending_queue_claims,
                    pending_turn_input_claims,
                    ..
                } = *driver;
                self.session = Some(session);
                self.abandon_queued_work_claims_after_local_abort(&err, &pending_queue_claims)
                    .await;
                self.abandon_turn_input_claims_after_local_abort(&err, &pending_turn_input_claims)
                    .await;
                return Err(err);
            }
            Err(err) => {
                let driver = driver.into_inner();
                self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                let RuntimeTurnDriver {
                    session,
                    pending_queue_claims,
                    pending_turn_input_claims,
                    ..
                } = *driver;
                self.session = Some(session);
                self.abandon_queued_work_claims_after_local_abort(&err, &pending_queue_claims)
                    .await;
                self.abandon_turn_input_claims_after_local_abort(&err, &pending_turn_input_claims)
                    .await;
                return Err(err);
            }
        };
        let driver = driver.into_inner();
        self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
        tracing::debug!(
            new_message_count = new_messages.len(),
            tool_call_count = assembler.tool_calls.len(),
            "runtime post-run_task"
        );

        let RuntimeTurnDriver {
            session,
            policy,
            turn_pipeline,
            llm_calls,
            pending_queue_claims,
            pending_turn_input_claims,
            ..
        } = *driver;
        self.session = Some(session);
        let pending_claims =
            LogicalTurnClaims::new(pending_queue_claims, pending_turn_input_claims);
        let finish_result = Box::pin(self.finish_turn(
            TurnFinishInput {
                turn_pipeline,
                assembler: assembler.with_llm_calls(llm_calls),
                new_messages,
                policy,
                turn_index,
                trace_turn_id,
            },
            &pending_claims,
            events,
            &finish_scoped_effect_controller,
            &cancel_state,
            session_execution_lease,
            session_execution_lease_release_policy,
            turn_control.as_ref(),
        ))
        .await;
        if let Err(err) = &finish_result {
            self.abandon_queued_work_claims_after_local_abort(err, &pending_claims.queued)
                .await;
            self.abandon_turn_input_claims_after_local_abort(err, &pending_claims.turn_inputs)
                .await;
        }
        finish_result
    }
    async fn normalize_input_items(
        &self,
        items: &[InputItem],
    ) -> Result<Vec<NormalizedItem>, String> {
        normalize_input_items(
            items,
            self.host.core.durability.attachment_store.as_ref(),
            self.host.core.attachment_source_policy.as_ref(),
        )
        .await
    }
}

pub fn ensure_durable_effect_input(input: &TurnInput) -> Result<(), RuntimeError> {
    if input.protocol_extension.is_some() {
        return Err(RuntimeError::new(
            RuntimeErrorCode::DurableEffectLiveProtocolExtension,
            "durable effect hosts do not support live protocol_extension inputs; encode replayable data in protocol_turn_options or persisted plugin state",
        ));
    }
    input
        .turn_context
        .live_plugin_inputs()
        .durable_effect_rejection()?;
    Ok(())
}

async fn emit_turn_activity_to_sink(events: &dyn TurnActivitySink, activity: TurnActivity) {
    if !events.is_noop() {
        events.emit(activity).await;
    }
}

async fn emit_turn_activity_to_sink_for_turn(
    events: &dyn TurnActivitySink,
    turn_id: &str,
    activity: TurnActivity,
) {
    if !events.is_noop() {
        events.emit_for_turn(turn_id, activity).await;
    }
}

/// Kind tag carried by a terminal diagnostic's error envelope.
#[derive(Clone, Copy)]
enum TerminalDiagnosticKind {
    /// The runtime itself refused to continue the turn.
    Runtime,
    /// Turn input failed normalization before any provider work.
    InputValidation,
    /// A plugin aborted the prepared turn.
    Plugin,
}

impl TerminalDiagnosticKind {
    fn as_envelope_kind(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::InputValidation => "input_validation",
            Self::Plugin => "plugin",
        }
    }
}

/// How a terminal diagnostic's turn activity is addressed to its sink.
enum TerminalActivityTarget<'a> {
    /// The sink is already turn-scoped, so the activity is emitted directly.
    TurnScopedSink(&'a dyn TurnActivitySink),
    /// The sink is unscoped, so the activity is addressed to `turn_id`.
    UnscopedSink {
        sink: &'a dyn TurnActivitySink,
        turn_id: &'a str,
    },
}

/// Typed diagnostic emitted immediately ahead of a terminal `TurnOutcome`.
struct TerminalDiagnostic<'a> {
    kind: TerminalDiagnosticKind,
    code: Option<String>,
    message: String,
    retryable: Option<bool>,
    activity: TerminalActivityTarget<'a>,
}

/// Emit the canonical terminal sequence for a stopped turn.
///
/// The order is fixed and load-bearing for host transcripts: the optional
/// diagnostic's session `Error` event (with its turn activity emitted in
/// between), then `TurnOutcome::Stopped(stop)`, then `Done`. Every session
/// event is recorded on `assembler` in emission order so the assembled turn
/// matches what the host streamed.
async fn emit_terminal_sequence(
    assembler: &mut TurnAssembler,
    events: &dyn EventSink,
    diagnostic: Option<TerminalDiagnostic<'_>>,
    stop: TurnStop,
) {
    if let Some(diagnostic) = diagnostic {
        let error_event = SessionStreamEvent::Error {
            message: diagnostic.message.clone(),
            envelope: Some(crate::session_model::ErrorEnvelope {
                kind: diagnostic.kind.as_envelope_kind().to_string(),
                code: diagnostic.code,
                terminal_reason: None,
                user_message: diagnostic.message.clone(),
                raw: None,
                retryable: diagnostic.retryable,
                provider_failure_kind: None,
            }),
        };
        assembler.push(&error_event);
        let activity = TurnActivity::independent(TurnEvent::Error {
            message: diagnostic.message,
        });
        match diagnostic.activity {
            TerminalActivityTarget::TurnScopedSink(sink) => {
                emit_turn_activity_to_sink(sink, activity).await;
            }
            TerminalActivityTarget::UnscopedSink { sink, turn_id } => {
                emit_turn_activity_to_sink_for_turn(sink, turn_id, activity).await;
            }
        }
        emit_session_event_to_sink(events, error_event).await;
    }
    let outcome_event = SessionStreamEvent::TurnOutcome {
        outcome: TurnOutcome::Stopped(stop),
    };
    assembler.push(&outcome_event);
    emit_session_event_to_sink(events, outcome_event).await;
    assembler.push(&SessionStreamEvent::Done);
    emit_session_event_to_sink(events, SessionStreamEvent::Done).await;
}

struct TurnScopedActivitySink<'a> {
    turn_id: String,
    inner: &'a dyn TurnActivitySink,
}

#[async_trait::async_trait]
impl TurnActivitySink for TurnScopedActivitySink<'_> {
    fn is_noop(&self) -> bool {
        self.inner.is_noop()
    }

    async fn emit(&self, activity: TurnActivity) {
        self.inner.emit_for_turn(&self.turn_id, activity).await;
    }
}

async fn publish_terminal_after_commit(
    turn_control: &ActiveTurnControl,
    resolver: &dyn AwaitEventResolver,
    terminal: &TurnTerminal,
    session_id: &str,
    turn_id: &str,
) {
    if let Err(err) = turn_control.publish_terminal(resolver, terminal).await {
        tracing::warn!(
            error = %err,
            session_id,
            turn_id,
            "turn committed but terminal publication failed"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_effect_loop(
    driver: &mut RuntimeTurnDriver<'_>,
    messages: crate::MessageSequence,
    event_tx: mpsc::Sender<RuntimeStreamEvent>,
    cancellation: CancellationToken,
    protocol_run_offset: usize,
    turn_control: Arc<ActiveTurnControl>,
    turn_control_host: Arc<dyn EffectHost>,
    cancel_controller: &dyn RuntimeEffectController,
    event_rx: &mut mpsc::Receiver<RuntimeStreamEvent>,
    assembler: &mut TurnAssembler,
    child_usage_event_relay: &ChildUsageEventRelay,
    events: &dyn EventSink,
    turn_events: &dyn TurnActivitySink,
) -> Result<(crate::MessageSequence, usize), RuntimeError> {
    // The start gate can change the handler's control flow before its first
    // effect, so durable runtimes must observe it through the handler-scoped
    // controller. That controller journals the observation and replays the
    // same answer after an owner crash. The shared controller is intentionally
    // reserved for the concurrent live watcher below: an out-of-band peek here
    // could observe a cancel that arrived after the original attempt and make
    // a replay take a different command path.
    let start_gate = crate::runtime::RuntimeNamedPhase::begin(
        driver.turn_phase_probe.clone(),
        "turn_cancel.start_gate",
    );
    let pending_cancel = await_turn_cancellation_start_gate(|| {
        turn_control.observe_pending_cancel(
            cancel_controller,
            crate::runtime::turn_control::TurnCancelPeekIdentity::StartGate,
        )
    })
    .await?;
    drop(start_gate);
    if pending_cancel.is_some() {
        cancellation.cancel();
    }
    let cancel_watcher = crate::task::spawn({
        let turn_control = Arc::clone(&turn_control);
        let cancellation = cancellation.clone();
        async move {
            if await_turn_cancellation_with_retry(|| {
                turn_control.await_cancel(turn_control_host.as_ref(), CancellationToken::new())
            })
            .await
            .is_some()
            {
                cancellation.cancel();
            }
        }
    });
    // Canonical future-size seam: `driver.run` is boxed exactly once here.
    // Driver growth is absorbed by this allocation instead of accreting
    // opportunistic boxes through the event-pump callers below.
    let run_future = Box::pin(driver.run(
        messages,
        event_tx,
        cancellation.clone(),
        protocol_run_offset,
    ));
    let result = drive_turn_to_completion(
        run_future,
        event_rx,
        assembler,
        child_usage_event_relay,
        events,
        turn_events,
    )
    .await;
    cancel_watcher.abort();
    result
}

const TURN_CANCEL_WATCH_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_millis(25);
const TURN_CANCEL_WATCH_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(1);
/// Keep the journaled turn-start observation bounded so a broken peek cannot
/// pin one Restate invocation forever. Exhaustion fails closed: the error
/// propagates and the turn fails without starting any effect (hosts classify
/// it as non-retryable, so the invocation retires as a failed turn). Transient
/// transport trouble does not reach this bound — a slow journaled peek stays
/// pending inside one attempt; only genuine terminal errors (revoked or
/// unknown keys) burn attempts.
const TURN_CANCEL_START_GATE_ATTEMPTS: usize = 3;

async fn await_turn_cancellation_start_gate<F, C>(
    mut watch: F,
) -> Result<Option<TurnCancellationEvidence>, RuntimeError>
where
    F: FnMut() -> C,
    C: std::future::Future<Output = Result<Option<TurnCancellationEvidence>, RuntimeError>>,
{
    let mut backoff = TURN_CANCEL_WATCH_RETRY_INITIAL;
    for attempt in 1..=TURN_CANCEL_START_GATE_ATTEMPTS {
        match watch().await {
            Ok(observation) => return Ok(observation),
            Err(err) if attempt == TURN_CANCEL_START_GATE_ATTEMPTS => return Err(err),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    attempt,
                    max_attempts = TURN_CANCEL_START_GATE_ATTEMPTS,
                    retry_after_ms = backoff.as_millis(),
                    "turn cancellation start gate failed; retrying before failing the invocation"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(TURN_CANCEL_WATCH_RETRY_MAX);
            }
        }
    }
    unreachable!("positive start-gate attempt limit")
}

async fn await_turn_cancellation_with_retry<F, C>(mut watch: F) -> Option<TurnCancellationEvidence>
where
    F: FnMut() -> C,
    C: std::future::Future<Output = Result<Option<TurnCancellationEvidence>, RuntimeError>>,
{
    let mut backoff = TURN_CANCEL_WATCH_RETRY_INITIAL;
    loop {
        match watch().await {
            Ok(observation) => return observation,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    retry_after_ms = backoff.as_millis(),
                    "turn cancellation watcher failed; retrying while the turn remains active"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(TURN_CANCEL_WATCH_RETRY_MAX);
            }
        }
    }
}

/// Pump the turn driver's event channel into the host sinks while the run
/// future executes, then drain any events emitted between completion and the
/// sender dropping.
///
/// Both the fresh and resumed turn entry points construct a
/// `RuntimeTurnDriver`, kick off its run future, and need identical
/// event-pump/drain behavior before tearing the driver down. Only the driver
/// construction and post-run teardown differ, so each caller owns those and
/// shares this loop.
async fn drive_turn_to_completion<F>(
    mut run_future: Pin<Box<F>>,
    event_rx: &mut mpsc::Receiver<RuntimeStreamEvent>,
    assembler: &mut TurnAssembler,
    child_usage_event_relay: &ChildUsageEventRelay,
    events: &dyn EventSink,
    turn_events: &dyn TurnActivitySink,
) -> Result<(crate::MessageSequence, usize), RuntimeError>
where
    F: std::future::Future<Output = Result<(crate::MessageSequence, usize), RuntimeError>> + ?Sized,
{
    let mut event_pump = RuntimeStreamEventPump {
        assembler,
        events,
        turn_events,
    };
    let run_result = drive_with_event_pump(
        run_future.as_mut(),
        event_rx,
        &mut event_pump,
        |pump, event| {
            Box::pin(async move {
                pump.emit(event).await;
            })
        },
    )
    .await;
    child_usage_event_relay.clear();
    while let Some(event) = event_rx.recv().await {
        emit_runtime_stream_event_to_sinks(events, turn_events, event, assembler).await;
    }
    run_result
}

struct RuntimeStreamEventPump<'pump> {
    assembler: &'pump mut TurnAssembler,
    events: &'pump dyn EventSink,
    turn_events: &'pump dyn TurnActivitySink,
}

impl RuntimeStreamEventPump<'_> {
    async fn emit(&mut self, event: RuntimeStreamEvent) {
        emit_runtime_stream_event_to_sinks(self.events, self.turn_events, event, self.assembler)
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_runtime_stream_event_to_sinks(
    events: &dyn EventSink,
    turn_events: &dyn TurnActivitySink,
    event: RuntimeStreamEvent,
    assembler: &mut TurnAssembler,
) {
    match event {
        RuntimeStreamEvent::Session(event) => {
            assembler.push(&event);
            emit_session_event_to_sink(events, event).await;
        }
        RuntimeStreamEvent::Turn(activity) => {
            emit_turn_activity_to_sink(turn_events, activity).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        ActiveTurnControl, TURN_CANCEL_START_GATE_ATTEMPTS, agent_frame_follow_turn_id,
        await_turn_cancellation_start_gate, await_turn_cancellation_with_retry,
        publish_terminal_after_commit,
    };
    use crate::{
        AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, ExecutionScope,
        InlineRuntimeEffectController, Resolution, ResolveOutcome, RuntimeError, TurnAddress,
        TurnCancellationEvidence, TurnFinish, TurnOutcome, TurnTerminal,
    };

    #[derive(Default)]
    struct RejectTerminalPublication {
        attempts: AtomicUsize,
        inline: InlineRuntimeEffectController,
    }

    #[async_trait::async_trait]
    impl AwaitEventResolver for RejectTerminalPublication {
        async fn await_event_key(
            &self,
            scope: &ExecutionScope,
            wait: AwaitEventWaitIdentity,
        ) -> Result<AwaitEventKey, RuntimeError> {
            self.inline.await_event_key(scope, wait).await
        }

        async fn resolve_await_event(
            &self,
            _key: &AwaitEventKey,
            _resolution: Resolution,
        ) -> Result<ResolveOutcome, RuntimeError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(RuntimeError::new(
                crate::RuntimeErrorCode::TransientTerminalPublication,
                "terminal backend unavailable",
            ))
        }

        async fn peek_await_event(
            &self,
            key: &AwaitEventKey,
        ) -> Result<Option<Resolution>, RuntimeError> {
            self.inline.peek_await_event(key).await
        }

        async fn await_await_event(
            &self,
            key: &AwaitEventKey,
            cancel: tokio_util::sync::CancellationToken,
            deadline: Option<std::time::Instant>,
        ) -> Result<Resolution, RuntimeError> {
            self.inline.await_await_event(key, cancel, deadline).await
        }

        async fn revoke_await_events_for_session(
            &self,
            session_id: &str,
        ) -> Result<(), RuntimeError> {
            self.inline
                .revoke_await_events_for_session(session_id)
                .await
        }

        async fn cancel_await_events_for_session(
            &self,
            session_id: &str,
        ) -> Result<(), RuntimeError> {
            self.inline
                .cancel_await_events_for_session(session_id)
                .await
        }
    }

    #[test]
    fn agent_frame_follow_turn_ids_are_distinct_and_deterministic() {
        assert_eq!(agent_frame_follow_turn_id("root-turn", 0), "root-turn");
        assert_eq!(
            agent_frame_follow_turn_id("root-turn", 1),
            "root-turn:agent-frame:1"
        );
        assert_eq!(
            agent_frame_follow_turn_id("root-turn", 2),
            "root-turn:agent-frame:2"
        );
    }

    #[tokio::test]
    async fn cancellation_watch_retries_transient_errors_until_evidence_arrives() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed_attempts = Arc::clone(&attempts);
        let evidence = await_turn_cancellation_with_retry(move || {
            let attempt = observed_attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 2 {
                    Err(RuntimeError::new(
                        crate::RuntimeErrorCode::TransientCancelWatch,
                        "temporary ingress failure",
                    ))
                } else {
                    Ok(Some(TurnCancellationEvidence {
                        request_id: "retry-request".to_string(),
                        origin: Some("test-user".to_string()),
                        reason: None,
                    }))
                }
            }
        })
        .await
        .expect("cancellation evidence after retries");

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(evidence.request_id, "retry-request");
    }

    #[tokio::test]
    async fn cancellation_start_gate_fails_after_bounded_retries() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed_attempts = Arc::clone(&attempts);
        let err = await_turn_cancellation_start_gate(move || {
            observed_attempts.fetch_add(1, Ordering::SeqCst);
            async {
                Err(RuntimeError::new(
                    crate::RuntimeErrorCode::CancelStartGateUnavailable,
                    "temporary ingress failure",
                ))
            }
        })
        .await
        .expect_err("start gate must fail closed after its retry budget");

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            TURN_CANCEL_START_GATE_ATTEMPTS
        );
        assert_eq!(err.code.to_string(), "cancel_start_gate_unavailable");
    }

    #[tokio::test]
    async fn terminal_publication_failure_is_non_fatal_after_commit() {
        let resolver = RejectTerminalPublication::default();
        let control = ActiveTurnControl::new(
            &resolver,
            TurnAddress::new("committed-session", "committed-turn"),
        )
        .await
        .expect("active turn control");
        publish_terminal_after_commit(
            &control,
            &resolver,
            &TurnTerminal::Committed {
                outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                    text: "committed".to_string(),
                }),
                cancellation: None,
                session_revision: Some(1),
            },
            "committed-session",
            "committed-turn",
        )
        .await;
        assert_eq!(resolver.attempts.load(Ordering::SeqCst), 1);
    }
}
