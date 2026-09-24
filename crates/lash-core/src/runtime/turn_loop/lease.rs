//! Session-execution lane custody and the claim hand-backs that follow a
//! local abort.
//!
//! Every turn phase runs under the lane this module acquires: the lease guard
//! itself, the fence a claim is written under, and the repairs a turn owes when
//! it dies before its commit could settle the rows it claimed.

use super::*;
use crate::TurnId;

/// Whether `candidate` is part of the logical execution recovered under
/// `resumable_turn_id`.
///
/// Keep this aligned with the active-input exclusion in
/// `store_backend_support::orphaned_active_turn_input_is_repairable`: closure
/// pins and the input they protect must make the same recovery decision.
pub(super) fn is_resumable_turn_or_follow_on(
    candidate: &TurnId,
    resumable_turn_id: &TurnId,
) -> bool {
    candidate
        .as_str()
        .strip_prefix(resumable_turn_id.as_str())
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(":agent-frame:"))
}

/// Which drive claim an aborted direct turn binds to itself (FIG-3589).
pub(super) enum DriveClaimToBind<'a> {
    /// The drive the turn holds, from its journaled drive effect.
    Claim(&'a crate::TurnInputClaim),
    /// The drive effect failed after its body may have claimed rows: whatever
    /// claim the accepted row carries under this lease generation.
    HeldUnder { generation: u64 },
}

struct SessionExecutionLaneProbe {
    store: Arc<dyn crate::store::RuntimePersistence>,
    session_id: SessionId,
    owner: crate::LeaseOwnerIdentity,
    executor_id: String,
    timings: crate::LeaseTimings,
    clock: Arc<dyn crate::Clock>,
}

#[async_trait::async_trait]
impl crate::QueuedLaneProbe for SessionExecutionLaneProbe {
    async fn try_acquire(&self) -> Result<crate::QueuedLaneAttempt, RuntimeError> {
        match SessionExecutionLeaseGuard::try_acquire_with_busy_holder(
            Arc::clone(&self.store),
            &self.session_id,
            &self.owner,
            &self.executor_id,
            self.timings,
            Arc::clone(&self.clock),
        )
        .await
        .map_err(super::runtime_error_from_store_commit)?
        {
            SessionExecutionLeaseGuardAcquisition::Acquired(guard) => Ok(
                crate::QueuedLaneAttempt::Acquired(crate::QueuedLaneGuard::new(guard)),
            ),
            SessionExecutionLeaseGuardAcquisition::Busy(holder) => Ok(
                crate::QueuedLaneAttempt::Busy(crate::QueuedLaneHolder::new(holder)),
            ),
        }
    }

    async fn pause(&self, slice: std::time::Duration) {
        self.clock.sleep(slice).await;
    }
}

impl LashRuntime {
    /// Claim and complete session-state admission before this turn starts.
    ///
    /// ADR 0077 makes this stricter than the CAS-only fallback: a busy lane may
    /// not hydrate or execute while another generation can migrate the complete
    /// mutable continuation.
    pub(super) async fn claim_session_execution_lease(
        &mut self,
    ) -> Result<Option<SessionExecutionLeaseGuard>, RuntimeError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(None);
        };
        match SessionExecutionLeaseGuard::try_acquire_for_executor(
            Arc::clone(&store),
            &self.state.session_id,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(super::runtime_error_from_store_commit)?
        {
            Some(guard) => {
                if store
                    .pending_queued_run(&self.state.session_id)
                    .await
                    .map_err(super::runtime_error_from_store_commit)?
                    .is_some()
                {
                    guard
                        .release_if_live()
                        .await
                        .map_err(super::runtime_error_from_store_commit)?;
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::QueuedRunPending,
                        "unfinished queued run owns this session",
                    ));
                }
                Ok(Some(guard))
            }
            None => Err(RuntimeError::new(
                RuntimeErrorCode::SessionExecutionLaneBusy,
                format!(
                    "session `{}` cannot start a turn until execution admission acquires its lease",
                    self.state.session_id
                ),
            )),
        }
    }

    /// Acquire the authoritative lane required to claim durable queued work.
    ///
    /// Ordinary controllers retain the public one-shot drain contract: Busy is
    /// reported as `None` and the durable row stays pending. A durable workflow
    /// controller instead applies the aliveness-aware policy in
    /// [`lane_wait`](super::native_substrate::lane_wait):
    /// wait out a crashed-looking holder's TTL and retry, but report the typed
    /// retryable [`RuntimeErrorCode::SessionExecutionLaneBusy`] the moment the
    /// holder proves it is alive or the wait budget elapses, so the engine's
    /// retry policy - not a sleep inside one invocation - paces the next
    /// attempt. Either way the foreign executor's lease authority is never
    /// bypassed or forged.
    ///
    /// The controller's [`acquire_queued_lane`](crate::AwaitEventResolver::acquire_queued_lane)
    /// operation owns that distinction: store-backed durable effect hosts keep
    /// the one-shot default, while an engine-re-driven handler overrides with
    /// the provided aliveness-aware wait.
    pub(super) async fn claim_session_execution_lease_for_queued_work(
        &mut self,
        opts: &QueuedTurnOptions<'_>,
    ) -> Result<Option<SessionExecutionLeaseGuard>, RuntimeError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(None);
        };
        let lane: Arc<dyn crate::QueuedLaneProbe> = Arc::new(SessionExecutionLaneProbe {
            store,
            session_id: self.state.session_id.clone(),
            owner: self.runtime_lease_owner.clone(),
            executor_id: self.runtime_lease_executor_id.clone(),
            timings: self.host.core.control.lease_timings,
            clock: Arc::clone(&self.host.core.clock),
        });
        match opts.source.acquire_lane(lane, opts.cancel.clone()).await? {
            crate::QueuedLaneAcquisition::Acquired(guard) => Ok(Some(guard.into_inner())),
            crate::QueuedLaneAcquisition::NotAcquired => Ok(None),
        }
    }

    pub(super) async fn settle_session_execution_lease<T>(
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
    // local pre-commit abort (a capture failure or a refused outcome
    // materialization, both before any durable write). Abandon clears
    // claim ownership, which both frees the rows for a peer and invalidates this
    // owner's pending completion. That is safe here because the turn is already
    // failing on the observed lease loss (ADR 0029).
    pub(super) async fn abandon_queued_work_claims_after_local_abort(
        &self,
        err: &RuntimeError,
        claims: &[crate::QueuedWorkClaim],
    ) {
        if self.queued_run.is_some() {
            return;
        }
        if !matches!(
            err.code,
            RuntimeErrorCode::SessionExecutionLeaseLost
                | RuntimeErrorCode::ExecutionStateCaptureFailed
                | RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
                | RuntimeErrorCode::AgentFrameSwitchAuthorConflict
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

    /// Record the turn's park when its abort is a refusal that parks it
    /// (FIG-3586, FIG-3600).
    ///
    /// A parked turn keeps every claim it holds, exactly as any live-fault
    /// abort does, so each redrive under the same build refuses again with
    /// nothing dispatched; the park is the typed, queryable record of why it
    /// stopped, which `drain_status` counts. Best effort, like every
    /// abort-path repair: the turn is already aborting and this must not
    /// replace its error. A park the store cannot write leaves the turn
    /// exactly as the abort left it — its held claims still keep the
    /// deployment from reporting drained.
    pub(super) async fn record_turn_park_after_abort(&self, err: &RuntimeError, turn_id: &TurnId) {
        let Some(reason) = crate::store::ParkReason::of_error(err) else {
            return;
        };
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return;
        };
        let write = crate::store::TurnParkWrite {
            session_id: self.state.session_id.clone(),
            turn_id: turn_id.clone(),
            reason,
            at_ms: self.host.core.clock.timestamp_ms(),
        };
        let reason_code = write.reason.code().as_str();
        let effect_kind = write.reason.effect_kind();
        match store.record_turn_park(&write).await {
            Ok(park) => {
                crate::operational_metrics::record_work_parked("turn", reason_code);
                tracing::warn!(
                    session_id = %self.state.session_id,
                    turn_id = %turn_id,
                    code = %err.code,
                    reason_code,
                    effect_kind,
                    park_id = %park.park_id,
                    attempts = park.attempts,
                    event = "turn.parked",
                    "turn parked on a replay refusal; redrive it under the build that wrote its \
                     journal, cancel it, or fork from before it"
                );
            }
            Err(error) => tracing::warn!(
                session_id = %self.state.session_id,
                turn_id = %turn_id,
                error = %error,
                event = "turn.park_record_failed",
                "failed to record the turn's park; its held claims still keep the deployment \
                 from draining"
            ),
        }
    }

    /// Hand claimed rows back after a local abort.
    ///
    /// A direct turn's journaled drive claim is never handed back: its redrive
    /// settles with the journaled claim token, so a handed-back row would cede
    /// the redrive and fold the input into a later turn. The direct turn binds
    /// that claim to itself instead
    /// ([`bind_drive_claim_after_abort`](Self::bind_drive_claim_after_abort)).
    pub(super) async fn abandon_turn_input_claims_after_local_abort(
        &self,
        err: &RuntimeError,
        claims: &[crate::TurnInputClaim],
    ) {
        if self.queued_run.is_some() {
            return;
        }
        let claims = claims
            .iter()
            .filter(|claim| !self.journaled_drive_claims.contains(&claim.claim_id))
            .cloned()
            .collect::<Vec<_>>();
        if !matches!(
            err.code,
            RuntimeErrorCode::SessionExecutionLeaseLost
                | RuntimeErrorCode::ExecutionStateCaptureFailed
                | RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
                | RuntimeErrorCode::AgentFrameSwitchAuthorConflict
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
        if let Err(abandon_err) = store.abandon_turn_input_claims(&claims).await {
            tracing::warn!(
                error = %abandon_err,
                claim_count = claims.len(),
                "failed to abandon turn input claims after local turn abort"
            );
        }
    }

    /// Bind an aborted direct turn's drive claim to that turn (FIG-3589,
    /// ADR 0069 §7).
    ///
    /// Runs on the abort path, before the lease is released, so no successor
    /// generation can reclaim the rows between the release and the binding.
    /// From here only the redrive of `turn_id` or a cancel of the input by the
    /// receipt the `Err` carries consumes them. A crashed turn never reaches
    /// this, so its claim still lapses with its generation and a successor
    /// recovers it (ADR 0029).
    ///
    /// Best effort, like every abort-path repair: the turn is already failing
    /// and this must not replace its error. A binding the store cannot write
    /// leaves the rows exactly as a crash leaves them, recoverable by the next
    /// generation, never lost.
    ///
    /// The binding is fenced by the claim's id and token. If the live fault was
    /// a lease loss and a successor generation already re-claimed the rows, the
    /// binding is a no-op and that successor may fold the input into its turn.
    pub(super) async fn bind_drive_claim_after_abort(
        &self,
        claim: DriveClaimToBind<'_>,
        receipt_input_id: &crate::InputId,
        turn_id: &TurnId,
    ) {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return;
        };
        let bound = match claim {
            DriveClaimToBind::Claim(claim) => {
                store
                    .bind_turn_input_claim(claim, turn_id, receipt_input_id)
                    .await
            }
            DriveClaimToBind::HeldUnder { generation } => {
                store
                    .bind_turn_input_claim_of_receipt(
                        &self.state.session_id,
                        receipt_input_id,
                        generation,
                        turn_id,
                    )
                    .await
            }
        };
        match bound {
            Ok(()) => tracing::debug!(
                session_id = %self.state.session_id,
                turn_id = %turn_id,
                receipt_input_id = %receipt_input_id,
                event = "turn_input.bound_to_aborted_turn",
                "bound the aborted direct turn's drive claim to the turn"
            ),
            Err(error) => tracing::warn!(
                session_id = %self.state.session_id,
                turn_id = %turn_id,
                receipt_input_id = %receipt_input_id,
                error = %error,
                event = "turn_input.bind_to_aborted_turn_failed",
                "failed to bind the aborted direct turn's drive claim; the next lease \
                 generation may reclaim it"
            ),
        }
    }

    /// Drain-time backstop for inputs no turn can deliver (FIG-1573).
    ///
    /// Runs only when the drain holds the session-execution lane and found
    /// nothing claimable - the wedge's exact signature - so a drain with work to
    /// do never touches a pending row.
    ///
    /// Why the sweep cannot hit a live turn: driving a turn takes `&mut self`,
    /// so this runtime has no turn of its own in flight here, and holding the
    /// lane means no other lane holder has one either - a peer runtime mints its
    /// own executor id, so it would be refused rather than admitted as reentry.
    /// The store re-validates the fence inside the repair's own transaction, so
    /// a lane displaced between the claim above and this call refuses the repair
    /// instead of clearing the new holder's claim columns.
    /// A turn running with no lane at all is unprotected by the lease by design
    /// (ADR 0029); for such a turn this only moves its pinned input to the next
    /// turn boundary, which changes delivery timing and never drops the input.
    ///
    /// The drain passes the turn id it is about to execute as
    /// `resumable_turn_id`, so a row pinned to a turn this very drain can still
    /// resume - the cold-recovery case, where the interrupted turn returns under
    /// the same turn id at a new generation - is excluded from the sweep. Its
    /// agent-frame follow-ons are excluded with it, because they are the same
    /// execution continuing.
    ///
    /// This is the only trigger that reaches a session already wedged by a
    /// pre-fix binary, or wedged inside a still-live process that keeps
    /// reentering its lane without ever minting a new generation.
    ///
    /// Accepted cost: a host may pin an input to a turn id *before* starting
    /// that turn, and an otherwise idle drain cannot tell that row apart from an
    /// orphan. Such a row is re-deferred and delivered at the next turn instead
    /// of the pre-named one - delivery timing, never a dropped input.
    pub(super) async fn defer_orphaned_turn_inputs_before_drain(
        &self,
        store: &Arc<dyn crate::store::RuntimePersistence>,
        fence: &crate::SessionExecutionLeaseAuthority,
        resumable_turn_id: &TurnId,
        scoped_effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<usize, RuntimeError> {
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_binding = turn_control_host
            .turn_control_binding(scoped_effect_controller)
            .await?;
        let settle_resumable_before_runtime_work = matches!(
            &turn_control_binding,
            crate::TurnControlBinding::HostOwned { .. }
        );
        let turn_control_resolver = turn_control_binding.resolver();
        let binding_id = turn_control_binding.binding_id();

        // The binding check and pending lookup are one fenced activation gate.
        // A reopened host with a different physical authority is refused before
        // commands, input acceptance, model calls, or other session work.
        let pending = store
            .pending_turn_cancel_closures(
                &self.state.session_id,
                fence,
                binding_id,
                &crate::runtime::effect::executor::admitted_turn_cancel_scope(
                    &crate::TurnAddress::new(&self.state.session_id, resumable_turn_id),
                    scoped_effect_controller.execution_scope(),
                    binding_id,
                ),
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let mut repaired_count = 0;
        for authorization in pending {
            let resumes_here =
                is_resumable_turn_or_follow_on(authorization.turn_id(), resumable_turn_id);
            if resumes_here && !settle_resumable_before_runtime_work {
                // The interrupted logical turn must replay to the original
                // closure position before issuing any of this authorization's
                // promise operations. Retain both its input and exact durable
                // authorization; final commit adopts and settles it there.
                continue;
            }
            let address = authorization.address();
            let control = crate::runtime::turn_control::ActiveTurnControl::new(
                turn_control_resolver,
                address.clone(),
            )
            .await?;
            let settlement = control
                .settle_authorized(turn_control_resolver, &authorization)
                .await?;
            if resumes_here {
                // Host-owned turn control has no invocation journal whose
                // prefix can replay this decision. Settle the predecessor's
                // exact proposal before fresh provider or tool work, while
                // retaining its input and pin for atomic final consumption.
                continue;
            }
            loop {
                let observed = store
                    .turn_cancel_request_intent(&address)
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
                match store
                    .repair_orphaned_active_turn_inputs(
                        &self.state.session_id,
                        fence,
                        authorization.turn_id(),
                        &observed,
                        Some(&settlement),
                    )
                    .await
                    .map_err(super::runtime_error_from_store_commit)?
                {
                    crate::TurnCancelRepairResult::Applied(outcome) => {
                        repaired_count += outcome.affected_inputs.len();
                        break;
                    }
                    crate::TurnCancelRepairResult::IntentChanged => continue,
                }
            }
        }

        let turn_ids = store
            .orphaned_active_turn_ids(
                &self.state.session_id,
                fence,
                crate::OrphanedTurnInputScope::LaneGeneration {
                    resumable_turn_id: Some(resumable_turn_id),
                },
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        for turn_id in turn_ids {
            let address = crate::TurnAddress::new(&self.state.session_id, &turn_id);
            'discover: loop {
                let observed = store
                    .turn_cancel_request_intent(&address)
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
                let settlement = match observed.request() {
                    Some(request) => {
                        let control = crate::runtime::turn_control::ActiveTurnControl::new(
                            turn_control_resolver,
                            address.clone(),
                        )
                        .await?;
                        let authorization = control.closure_authorization(
                            binding_id,
                            crate::runtime::effect::executor::admitted_turn_cancel_scope(
                                &address,
                                scoped_effect_controller.execution_scope(),
                                binding_id,
                            ),
                            fence,
                            observed.clone(),
                            true,
                            Some(request.evidence()),
                        )?;
                        match store
                            .authorize_turn_cancel_closure(fence, &authorization)
                            .await
                        {
                            Ok(_) => {}
                            Err(crate::StoreError::TurnCancelIntentChanged { .. }) => continue,
                            Err(error) => {
                                return Err(super::runtime_error_from_store_commit(error));
                            }
                        }
                        let settlement = control
                            .settle_authorized(turn_control_resolver, &authorization)
                            .await?;
                        Some(settlement)
                    }
                    None => None,
                };
                let mut repair_observed = observed;
                loop {
                    match store
                        .repair_orphaned_active_turn_inputs(
                            &self.state.session_id,
                            fence,
                            &turn_id,
                            &repair_observed,
                            settlement.as_ref(),
                        )
                        .await
                    {
                        Ok(crate::TurnCancelRepairResult::Applied(outcome)) => {
                            repaired_count += outcome.affected_inputs.len();
                            break 'discover;
                        }
                        Ok(crate::TurnCancelRepairResult::IntentChanged)
                            if settlement.is_some() =>
                        {
                            // The exact promise operation is already pinned and
                            // may not be overwritten. Refresh only the store CAS
                            // predicate and finish the authenticated winner.
                            repair_observed = store
                                .turn_cancel_request_intent(&address)
                                .await
                                .map_err(super::runtime_error_from_store_commit)?;
                        }
                        Ok(crate::TurnCancelRepairResult::IntentChanged) => continue 'discover,
                        Err(err) => return Err(super::runtime_error_from_store_commit(err)),
                    }
                }
            }
        }
        Ok(repaired_count)
    }

    /// Re-defer the inputs a torn-down turn can no longer deliver (FIG-1573).
    ///
    /// A turn's final commit carries this repair for the inputs it did not
    /// deliver, so a turn that ends *without* committing owes it here instead.
    /// Nothing else can: the rows are addressed only by the dead turn's id, and
    /// no later turn will ever carry that id again.
    ///
    /// A turn that held no lane cannot repair anything here, and a turn whose lane has already
    /// been taken over must not: the new holder resumes this same turn id and delivers those
    /// rows itself.
    /// Both cases fall through to the drain backstop
    /// ([`crate::OrphanedTurnInputScope::LaneGeneration`]) for rows no live generation claims -
    /// an accepted row still claimed at the live generation stays put until that generation is
    /// superseded, which is exactly the row the resuming holder owns.
    ///
    /// Best-effort by construction - the turn is already failing and this repair
    /// must not replace its error.
    pub(in crate::runtime) async fn defer_orphaned_turn_inputs_after_teardown(
        &self,
        trace_turn_id: &TurnId,
        session_execution_lease: Option<&crate::SessionExecutionLeaseAuthority>,
        scoped_effect_controller: &crate::ScopedEffectController<'_>,
    ) {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return;
        };
        let Some(fence) = session_execution_lease else {
            tracing::debug!(
                session_id = %self.state.session_id,
                turn_id = %trace_turn_id,
                event = "turn_input.defer_after_teardown_skipped",
                "a lane-less turn leaves its orphaned inputs to the drain backstop"
            );
            return;
        };
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_binding = match turn_control_host
            .turn_control_binding(scoped_effect_controller)
            .await
        {
            Ok(binding) => binding,
            Err(err) => {
                tracing::warn!(session_id = %self.state.session_id, turn_id = %trace_turn_id, error = %err, event = "turn_input.cancel_gate_binding_failed");
                return;
            }
        };
        let turn_control_resolver = turn_control_binding.resolver();
        let binding_id = turn_control_binding.binding_id();
        let address = crate::TurnAddress::new(&self.state.session_id, trace_turn_id);
        loop {
            let observed = match store.turn_cancel_request_intent(&address).await {
                Ok(observed) => observed,
                Err(err) => {
                    tracing::warn!(session_id = %self.state.session_id, turn_id = %trace_turn_id, error = %err, event = "turn_input.cancel_intent_read_failed");
                    return;
                }
            };
            let observed_decision =
                match crate::runtime::turn_control::ActiveTurnControl::peek_orphan_repair_decision(
                    turn_control_resolver,
                    &address,
                )
                .await
                {
                    Ok(Some(decision)) => decision,
                    Ok(None) => return,
                    Err(err) => {
                        tracing::warn!(session_id = %self.state.session_id, turn_id = %trace_turn_id, error = %err, event = "turn_input.cancel_gate_peek_failed");
                        return;
                    }
                };
            if observed_decision == crate::TurnCancelRepairDecision::NoCancellationIntent {
                match store
                    .repair_orphaned_active_turn_inputs(
                        &self.state.session_id,
                        fence,
                        trace_turn_id,
                        &observed,
                        None,
                    )
                    .await
                {
                    Ok(crate::TurnCancelRepairResult::IntentChanged) => continue,
                    Ok(crate::TurnCancelRepairResult::Applied(repaired)) => {
                        if !repaired.is_empty() {
                            tracing::info!(
                                session_id = %self.state.session_id,
                                turn_id = %trace_turn_id,
                                repaired = repaired.len(),
                                event = "turn_input.deferred_after_teardown",
                                "re-deferred active-turn inputs without sealing an unresolved cancellation gate"
                            );
                        }
                        return;
                    }
                    Err(crate::store::StoreError::SessionExecutionLeaseExpired { .. }) => {
                        tracing::debug!(
                            session_id = %self.state.session_id,
                            turn_id = %trace_turn_id,
                            event = "turn_input.defer_after_teardown_fenced",
                            "a superseded lane leaves the torn-down turn's inputs to its successor"
                        );
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(
                            session_id = %self.state.session_id,
                            turn_id = %trace_turn_id,
                            error = %err,
                            event = "turn_input.defer_after_teardown_failed",
                            "failed to re-defer active-turn inputs after a turn ended without committing"
                        );
                        return;
                    }
                }
            }
            let (cancelled, evidence) = match observed_decision {
                crate::TurnCancelRepairDecision::CancellationWon(evidence) => {
                    (true, Some(evidence))
                }
                crate::TurnCancelRepairDecision::CancellationDidNotWin => (false, None),
                crate::TurnCancelRepairDecision::NoCancellationIntent => unreachable!(
                    "no-intent teardown repair returned before cancellation closure authorization"
                ),
            };
            let control = match crate::runtime::turn_control::ActiveTurnControl::new(
                turn_control_resolver,
                address.clone(),
            )
            .await
            {
                Ok(control) => control,
                Err(err) => {
                    tracing::warn!(session_id = %self.state.session_id, turn_id = %trace_turn_id, error = %err, event = "turn_input.cancel_gate_prepare_failed");
                    return;
                }
            };
            let authorization = match control.closure_authorization(
                binding_id,
                crate::runtime::effect::executor::admitted_turn_cancel_scope(
                    &address,
                    scoped_effect_controller.execution_scope(),
                    binding_id,
                ),
                fence,
                observed.clone(),
                cancelled,
                evidence,
            ) {
                Ok(authorization) => authorization,
                Err(err) => {
                    tracing::warn!(session_id = %self.state.session_id, turn_id = %trace_turn_id, error = %err, event = "turn_input.cancel_closure_assembly_failed");
                    return;
                }
            };
            match store
                .authorize_turn_cancel_closure(fence, &authorization)
                .await
            {
                Ok(_) => {}
                Err(crate::StoreError::TurnCancelIntentChanged { .. }) => continue,
                Err(err) => {
                    tracing::warn!(session_id = %self.state.session_id, turn_id = %trace_turn_id, error = %err, event = "turn_input.cancel_closure_authorization_failed");
                    return;
                }
            }
            let settlement = match control
                .settle_authorized(turn_control_resolver, &authorization)
                .await
            {
                Ok(settlement) => settlement,
                Err(err) => {
                    tracing::warn!(session_id = %self.state.session_id, turn_id = %trace_turn_id, error = %err, event = "turn_input.cancel_gate_settlement_failed");
                    return;
                }
            };
            let mut repair_observed = observed;
            loop {
                match store
                    .repair_orphaned_active_turn_inputs(
                        &self.state.session_id,
                        fence,
                        trace_turn_id,
                        &repair_observed,
                        Some(&settlement),
                    )
                    .await
                {
                    Ok(crate::TurnCancelRepairResult::IntentChanged) => {
                        repair_observed = match store.turn_cancel_request_intent(&address).await {
                            Ok(observed) => observed,
                            Err(err) => {
                                tracing::warn!(session_id = %self.state.session_id, turn_id = %trace_turn_id, error = %err, event = "turn_input.cancel_intent_refresh_failed");
                                return;
                            }
                        };
                    }
                    Ok(crate::TurnCancelRepairResult::Applied(repaired)) if repaired.is_empty() => {
                        return;
                    }
                    Ok(crate::TurnCancelRepairResult::Applied(repaired)) => {
                        tracing::info!(
                        session_id = %self.state.session_id,
                        turn_id = %trace_turn_id,
                        repaired = repaired.len(),
                        event = "turn_input.deferred_after_teardown",
                        "re-deferred active-turn inputs pinned to a turn that ended without committing"
                            );
                        return;
                    }
                    // A fence refusal is the ordinary outcome for a turn whose lane was
                    // taken over: the repair is the new holder's, not ours.
                    Err(crate::store::StoreError::SessionExecutionLeaseExpired { .. }) => {
                        tracing::debug!(
                        session_id = %self.state.session_id,
                        turn_id = %trace_turn_id,
                        event = "turn_input.defer_after_teardown_fenced",
                        "a superseded lane leaves the torn-down turn's inputs to its successor"
                        );
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(
                            session_id = %self.state.session_id,
                            turn_id = %trace_turn_id,
                            error = %err,
                            event = "turn_input.defer_after_teardown_failed",
                            "failed to re-defer active-turn inputs after a turn ended without committing"
                        );
                        return;
                    }
                }
            }
        }
    }
}
