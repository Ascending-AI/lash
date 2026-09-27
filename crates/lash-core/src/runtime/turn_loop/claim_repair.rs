//! Claim handbacks and orphan repair under drive admission.

use super::*;
use crate::TurnId;

pub(super) fn is_resumable_turn_or_follow_on(
    candidate: &TurnId,
    resumable_turn_id: &TurnId,
) -> bool {
    candidate
        .as_str()
        .strip_prefix(resumable_turn_id.as_str())
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(":agent-frame:"))
}

impl LashRuntime {
    pub(in crate::runtime) async fn claim_drive_authority(
        &mut self,
    ) -> Result<Option<DriveClaimGuard>, RuntimeError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(None);
        };
        let guard = DriveClaimGuard::try_acquire_for_executor(
            store,
            &self.state.session_id,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(super::runtime_error_from_store_commit)?;
        Ok(guard)
    }

    pub(super) async fn claim_drive_authority_for_queued_work(
        &mut self,
        _opts: &QueuedTurnOptions<'_>,
    ) -> Result<Option<DriveClaimGuard>, RuntimeError> {
        Ok(self.drive_root.as_ref().map(|root| {
            DriveClaimGuard::from_drive_fence(
                &root.fence,
                self.runtime_lease_owner.clone(),
                self.runtime_lease_executor_id.clone(),
            )
        }))
    }

    pub(in crate::runtime) async fn settle_drive_authority<T>(
        &self,
        _guard: Option<&DriveClaimGuard>,
        result: Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        result
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

    /// Hand claimed rows back after a local abort.
    ///
    /// A root's recorded claim is never handed back: its redrive settles with
    /// the recorded claim token, so a handed-back row would cede the redrive.
    /// The row stays claimed, and the session's next drive admits the same
    /// root first (FIG-3600).
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
    /// The follow-on the durable head owes, read once per repair pass.
    async fn owed_follow_on(
        store: &Arc<dyn crate::store::RuntimePersistence>,
        owed: &mut Option<Option<crate::store::PendingFollowOn>>,
    ) -> Result<Option<crate::store::PendingFollowOn>, RuntimeError> {
        if let Some(owed) = owed {
            return Ok(owed.clone());
        }
        let read = store
            .load_session_head_meta()
            .await
            .map_err(super::runtime_error_from_store_commit)?
            .and_then(|head| head.pending_follow_on);
        *owed = Some(read.clone());
        Ok(read)
    }

    pub(in crate::runtime) async fn defer_orphaned_turn_inputs_before_drain(
        store: &Arc<dyn crate::store::RuntimePersistence>,
        fence: &crate::ClaimAuthority,
        resumable_turn_id: &TurnId,
        scoped_effect_controller: &crate::ScopedEffectController<'_>,
        session_id: &SessionId,
        turn_control_host: &dyn crate::EffectHost,
    ) -> Result<usize, RuntimeError> {
        let turn_control_binding = turn_control_host
            .turn_control_binding(scoped_effect_controller)
            .await?;
        let turn_control_resolver = turn_control_binding.resolver();
        let binding_id = turn_control_binding.binding_id();

        // The binding check and pending lookup are one fenced activation gate.
        // A reopened host with a different physical authority is refused before
        // commands, input acceptance, model calls, or other session work.
        let pending = store
            .pending_turn_cancel_closures(
                session_id,
                fence,
                binding_id,
                &crate::runtime::effect::executor::admitted_turn_cancel_scope(
                    &crate::TurnAddress::new(session_id, resumable_turn_id),
                    scoped_effect_controller.execution_scope(),
                    binding_id,
                ),
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let mut repaired_count = 0;
        // The follow-on the head owes is not orphaned: its turn runs next, and
        // the input pinned to it is its own (ADR 0101 §3). The head is read
        // only when some turn looks orphaned.
        let mut owed: Option<Option<crate::store::PendingFollowOn>> = None;
        for authorization in pending {
            let resumes_here =
                is_resumable_turn_or_follow_on(authorization.turn_id(), resumable_turn_id)
                    || Self::owed_follow_on(store, &mut owed)
                        .await?
                        .is_some_and(|owed| owed.is_turn(authorization.turn_id()));
            if resumes_here {
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
                .settle_authorized(turn_control_resolver, &authorization, None)
                .await?;
            loop {
                let observed = store
                    .turn_cancel_request_intent(&address)
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
                match store
                    .repair_orphaned_active_turn_inputs(
                        session_id,
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
                session_id,
                fence,
                crate::OrphanedTurnInputScope::LaneGeneration {
                    resumable_turn_id: Some(resumable_turn_id),
                },
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let owed = if turn_ids.is_empty() {
            None
        } else {
            Self::owed_follow_on(store, &mut owed).await?
        };
        for turn_id in turn_ids
            .into_iter()
            .filter(|turn_id| !owed.as_ref().is_some_and(|owed| owed.is_turn(turn_id)))
        {
            let address = crate::TurnAddress::new(session_id, &turn_id);
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
                            None,
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
                            .settle_authorized(turn_control_resolver, &authorization, None)
                            .await?;
                        Some(settlement)
                    }
                    None => None,
                };
                let mut repair_observed = observed;
                loop {
                    match store
                        .repair_orphaned_active_turn_inputs(
                            session_id,
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
        session_execution_lease: Option<&crate::ClaimAuthority>,
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
                    Err(crate::store::StoreError::StaleDriveFence { .. }) => {
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
            let evidence = match observed_decision {
                crate::TurnCancelRepairDecision::CancellationWon(evidence) => Some(evidence),
                crate::TurnCancelRepairDecision::CancellationDidNotWin => None,
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
                None,
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
                .settle_authorized(turn_control_resolver, &authorization, None)
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
                    Err(crate::store::StoreError::StaleDriveFence { .. }) => {
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
