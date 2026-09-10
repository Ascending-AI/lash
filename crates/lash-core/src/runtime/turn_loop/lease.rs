//! Session-execution lane custody and the claim hand-backs that follow a
//! local abort.
//!
//! Every turn phase runs under the lane this module acquires: the lease guard
//! itself, the fence a claim is written under, and the repairs a turn owes when
//! it dies before its commit could settle the rows it claimed.

use super::*;
use crate::TurnId;

struct SessionExecutionLaneProbe {
    store: Arc<dyn crate::store::RuntimePersistence>,
    session_id: String,
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
        .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()))?
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
            store,
            &self.state.session_id,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()))?
        {
            Some(guard) => Ok(Some(guard)),
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
        opts: &TurnOptions<'_>,
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
        match opts
            .scoped_effect_controller()
            .controller()
            .acquire_queued_lane(lane, opts.cancel.clone())
            .await?
        {
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
        if !matches!(
            err.code,
            RuntimeErrorCode::SessionExecutionLeaseLost
                | RuntimeErrorCode::ExecutionStateCaptureFailed
                | RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
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
    /// Unclaimed rows are skipped by construction: they hold no claim to
    /// release, so an aborted unclaimed drive already leaves its acceptance
    /// exactly where a drain expects to find it (ADR 0069 §5).
    pub(super) async fn abandon_turn_input_claims_after_local_abort(
        &self,
        err: &RuntimeError,
        drives: &[super::turn_input_ingress::TurnInputDrive],
    ) {
        let claims = drives
            .iter()
            .filter_map(super::turn_input_ingress::TurnInputDrive::as_claim)
            .cloned()
            .collect::<Vec<_>>();
        if !matches!(
            err.code,
            RuntimeErrorCode::SessionExecutionLeaseLost
                | RuntimeErrorCode::ExecutionStateCaptureFailed
                | RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
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

    /// The row set a replayed acceptance must redrive to re-derive its turn.
    ///
    /// A replayed acceptance whose row is already settled redrives the turn so
    /// the store recognises the commit identity and replays its receipt
    /// (ADR 0069 §6). That only works if the redrive materializes the same
    /// words the first execution did, and the first execution may have absorbed
    /// every earlier claimed row into the same turn. The durable applications
    /// name that set: every row the settled row's turn applied, in durable
    /// commit order. Each of them is settled by construction, so the same
    /// no-op cancel probe the drive site already uses reads them back without
    /// withdrawing anything.
    ///
    /// Application evidence also records *where* each row entered the turn. A
    /// direct acceptance redrive reconstructs only the rows applied at the
    /// same checkpoint as that acceptance (normally the initial `None` group);
    /// checkpoint effects replay their own journaled claim sets later. Folding
    /// a checkpoint-applied row into this initial set changes both the message
    /// shape and the semantic commit identity.
    ///
    /// Reconstruction is fail-closed. Falling back to the settled row alone is
    /// only sound when it was the whole initial group, which cannot be proven
    /// without the durable applications. Refuse before executing the turn and
    /// tell the operator which history must be restored instead of allowing a
    /// bare commit-identity mismatch after provider work.
    pub(super) async fn settled_turn_input_redrive_set(
        &self,
        store: &dyn crate::store::RuntimePersistence,
        settled: &crate::PendingTurnInput,
    ) -> Result<crate::UnclaimedTurnInputs, RuntimeError> {
        let unavailable = |detail: String| {
            RuntimeError::new(
                RuntimeErrorCode::TurnInputRedriveSetUnavailable,
                format!(
                    "cannot rebuild redrive set for settled turn input `{}` in session `{}`: \
                     {detail}; operator recovery: restore turn-input application history, then \
                     redrive the same turn",
                    settled.input_id, self.state.session_id
                ),
            )
        };
        let applications = store
            .list_turn_input_applications(&self.state.session_id)
            .await
            .map_err(|err| unavailable(format!("application history read failed: {err}")))?;
        let Some(settled_application) = applications
            .iter()
            .find(|application| application.input_id == settled.input_id)
            .cloned()
        else {
            return Err(unavailable(
                "the settled input has no durable application record".to_string(),
            ));
        };
        let redrive_applications = applications
            .iter()
            .filter(|application| {
                application.turn_id == settled_application.turn_id
                    && application.checkpoint == settled_application.checkpoint
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut inputs = Vec::with_capacity(redrive_applications.len());
        for application in &redrive_applications {
            if application.input_id == settled.input_id {
                inputs.push(settled.clone());
                continue;
            }
            match store
                .cancel_pending_turn_input(&self.state.session_id, &application.input_id)
                .await
            {
                Ok(crate::PendingTurnInputCancelOutcome::AlreadyCompleted(sibling)) => {
                    inputs.push(sibling);
                }
                Ok(outcome) => {
                    return Err(unavailable(format!(
                        "application sibling `{}` was not completed ({outcome:?})",
                        application.input_id
                    )));
                }
                Err(err) => {
                    return Err(unavailable(format!(
                        "application sibling `{}` could not be read: {err}",
                        application.input_id
                    )));
                }
            }
        }
        tracing::debug!(
            session_id = %self.state.session_id,
            turn_id = %settled_application.turn_id,
            settled_input_id = %settled.input_id,
            checkpoint = ?settled_application.checkpoint,
            sibling_count = inputs.len() - 1,
            event = "turn_input.redrive_set_recovered",
            "replayed acceptance redrives the row set applied at its original checkpoint"
        );
        Ok(crate::UnclaimedTurnInputs {
            session_id: self.state.session_id.clone(),
            inputs,
            applications: redrive_applications,
        })
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
    ) -> usize {
        match store
            .defer_orphaned_active_turn_inputs(
                &self.state.session_id,
                fence,
                crate::OrphanedTurnInputScope::LaneGeneration {
                    resumable_turn_id: Some(resumable_turn_id),
                },
            )
            .await
        {
            Ok(repaired) if repaired.is_empty() => 0,
            Ok(repaired) => {
                tracing::info!(
                    session_id = %self.state.session_id,
                    repaired = repaired.len(),
                    live_generation = fence.fencing_token,
                    event = "turn_input.deferred_before_drain",
                    "re-deferred active-turn inputs pinned to turns that can no longer commit"
                );
                repaired.len()
            }
            // The lane went out from under this drain; whoever holds it now owns
            // the repair, and the drain's own next store call reports the loss.
            Err(crate::store::StoreError::SessionExecutionLeaseExpired { .. }) => {
                tracing::debug!(
                    session_id = %self.state.session_id,
                    event = "turn_input.defer_before_drain_fenced",
                    "a superseded lane leaves the orphan repair to its successor"
                );
                0
            }
            Err(err) => {
                tracing::warn!(
                    session_id = %self.state.session_id,
                    error = %err,
                    event = "turn_input.defer_before_drain_failed",
                    "failed to re-defer orphaned active-turn inputs before a queued-work drain"
                );
                0
            }
        }
    }

    /// Re-defer the inputs a torn-down turn can no longer deliver (FIG-1573).
    ///
    /// A turn's final commit carries this repair for the inputs it did not
    /// deliver, so a turn that ends *without* committing owes it here instead.
    /// Nothing else can: the rows are addressed only by the dead turn's id, and
    /// no later turn will ever carry that id again.
    ///
    /// Runs under the authority the dying turn itself held. A turn that held no
    /// lane cannot repair anything here, and a turn whose lane has already been
    /// taken over must not: the new holder resumes this same turn id and
    /// delivers those rows itself. Both cases fall through to the drain backstop
    /// ([`crate::OrphanedTurnInputScope::LaneGeneration`]) for rows no live
    /// generation claims - an accepted row still claimed at the live generation
    /// stays put until that generation is superseded, which is exactly the row
    /// the resuming holder owns.
    ///
    /// Best-effort by construction - the turn is already failing and this repair
    /// must not replace its error.
    pub(in crate::runtime) async fn defer_orphaned_turn_inputs_after_teardown(
        &self,
        trace_turn_id: &TurnId,
        session_execution_lease: Option<&crate::SessionExecutionLeaseAuthority>,
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
        match store
            .defer_orphaned_active_turn_inputs(
                &self.state.session_id,
                fence,
                crate::OrphanedTurnInputScope::Turn(trace_turn_id),
            )
            .await
        {
            Ok(repaired) if repaired.is_empty() => {}
            Ok(repaired) => tracing::info!(
                session_id = %self.state.session_id,
                turn_id = %trace_turn_id,
                repaired = repaired.len(),
                event = "turn_input.deferred_after_teardown",
                "re-deferred active-turn inputs pinned to a turn that ended without committing"
            ),
            // A fence refusal is the ordinary outcome for a turn whose lane was
            // taken over: the repair is the new holder's, not ours.
            Err(crate::store::StoreError::SessionExecutionLeaseExpired { .. }) => {
                tracing::debug!(
                    session_id = %self.state.session_id,
                    turn_id = %trace_turn_id,
                    event = "turn_input.defer_after_teardown_fenced",
                    "a superseded lane leaves the torn-down turn's inputs to its successor"
                )
            }
            Err(err) => tracing::warn!(
                session_id = %self.state.session_id,
                turn_id = %trace_turn_id,
                error = %err,
                event = "turn_input.defer_after_teardown_failed",
                "failed to re-defer active-turn inputs after a turn ended without committing"
            ),
        }
    }
}
