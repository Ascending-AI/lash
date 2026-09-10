//! Turn ingress: accepting a host turn as durable admission evidence and
//! claiming the row it just wrote (ADR 0069).
//!
//! Everything here runs before the prepare phase and hands it a claim, an
//! execution lane, and the input the claim actually materialized.

use super::*;
use crate::TurnId;

impl LashRuntime {
    /// Run one logical turn and stream every physical frame to the host sink.
    /// Accept `input` as durable admission evidence, then drive it to a
    /// terminal turn ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)).
    ///
    /// Identical to [`stream_turn_with_agent_frames`](Self::stream_turn_with_agent_frames)
    /// except that it returns only the run's terminal physical turn.
    pub async fn stream_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        self.stream_turn_with_agent_frames(input, opts)
            .await
            .map(|run| {
                run.into_final_turn()
                    .expect("logical turn always contains a terminal physical turn")
            })
    }

    pub(in crate::runtime) async fn stream_turn_with_scoped_effect_controller_inner(
        &mut self,
        context: TurnPrepareContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let TurnPrepareContext {
            mut input,
            sinks: TurnSinks {
                events,
                turn_events,
            },
            scoped_effect_controller,
            cancel,
            queued_claims,
            turn_input_claims,
            materialize_initial_claims,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
        } = context;
        if queued_claims.is_empty()
            && turn_input_claims.is_empty()
            && let Some(lease) = session_execution_lease
        {
            while self
                .drain_next_session_command_with_cancellation(
                    &lease.fence(),
                    cancel.clone(),
                    scoped_effect_controller.controller(),
                )
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
            .get_or_insert_with(|| TurnId::from(scoped_effect_controller.scope_id()))
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
        Box::pin(self.stream_turn_inner(TurnPrepareContext {
            input: input.clone(),
            sinks: TurnSinks {
                events,
                turn_events,
            },
            scoped_effect_controller,
            cancel: cancel.clone(),
            queued_claims,
            turn_input_claims,
            materialize_initial_claims,
            lease: TurnLeaseScope {
                guard: session_execution_lease,
                release_policy: session_execution_lease_release_policy,
            },
        }))
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
    ///
    /// It is also where a turn is *accepted*: `input` becomes durable admission
    /// evidence before it is driven
    /// ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)).
    ///
    /// This is the sole turn ingress. The call first commits `input` as a
    /// `NextTurn` Pending Turn Input row — the same admission evidence
    /// [`enqueue_turn_input`](Self::enqueue_turn_input) writes
    /// ([ADR 0010](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0010-pending-turn-input-is-admission-evidence.md))
    /// — and only then claims that row under the generation-fenced claim
    /// machinery of
    /// [ADR 0029](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0029-claims-are-generation-fenced-under-the-session-lease.md)
    /// and drives it. The acceptance identity rides back on
    /// [`AgentFrameRun::acceptance`] and on every assembled turn's
    /// [`AssembledTurn::turn_input_acceptance`].
    ///
    /// Consequences a caller must plan for:
    ///
    /// * **Durable backends pay one extra store commit per turn.** That is the
    ///   acceptance commit, and it is stated rather than gated.
    /// * **This caller is the first driver, not the owner.** Dropping the
    ///   returned future does not stop the turn; abandonment is expressed by
    ///   cancelling the accepted input, never by silence.
    /// * **The claim absorbs whatever else is already queued.** A direct turn
    ///   claims the head of the pending next-turn queue exactly as a drain
    ///   does, so inputs enqueued earlier join this turn instead of waiting for
    ///   another one.
    /// * **Live per-turn context stays with this caller.** `protocol_extension`
    ///   and live `TurnContext` plugin inputs are process-local and cannot be
    ///   persisted, so a worker that recovers this accepted row drives its
    ///   durable projection.
    /// * **No new idempotency machinery.** A retry after an unacknowledged
    ///   crash is a new turn; a caller that needs at-most-once submission names
    ///   its own `source_key` through [`enqueue_turn_input`](Self::enqueue_turn_input).
    ///
    /// A store-less runtime has no store to accept into and drives `input`
    /// directly; it is the one configuration with no durable ingress at all.
    pub async fn stream_turn_with_agent_frames(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        use futures_util::FutureExt;

        // Keep the guard outside the unwinding body. Both streamed facade turns
        // and managed child turns pass here, so their task JoinError cannot
        // surface before owner-side release has finished. Cancellation still
        // drops the guard and uses its best-effort cleanup / TTL fallback.
        let mut lease = None;
        let result = std::panic::AssertUnwindSafe(
            self.stream_turn_with_agent_frames_holding_lease(input, opts, &mut lease)
                .boxed(),
        )
        .catch_unwind()
        .await;
        match result {
            Ok(result) => result,
            Err(payload) => {
                if let Some(lease) = lease.as_ref()
                    && let Err(error) = lease.release_if_live().await
                {
                    tracing::warn!(%error, "failed to release session execution lease after turn panic");
                }
                std::panic::resume_unwind(payload)
            }
        }
    }

    async fn stream_turn_with_agent_frames_holding_lease(
        &mut self,
        mut input: TurnInput,
        opts: TurnOptions<'_>,
        session_execution_lease: &mut Option<SessionExecutionLeaseGuard>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        if let Some(hint) = opts.local_cancel_origin_hint() {
            input.turn_context.set_local_cancel_origin_hint(hint);
        }
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let cancel = opts.cancel.clone();
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            *session_execution_lease = self.claim_session_execution_lease().await?;
            let scoped_effect_controller = opts.scoped_effect_controller();
            let result = Box::pin(self.drive_logical_turn(
                LogicalTurnStart::Input(input),
                opts.events_or_noop(),
                opts.turn_events_or_noop(),
                scoped_effect_controller,
                cancel,
                LogicalTurnClaims::new(Vec::new(), Vec::new()),
                session_execution_lease,
                stopwatch,
            ))
            .await;
            return self
                .settle_session_execution_lease(session_execution_lease.as_ref(), result)
                .await;
        };

        // The row is minted exactly as a queued admission is — no source key and
        // no derived identity — so direct ingress inherits the queue's identity
        // semantics rather than introducing a second, turn-shaped one.
        let trace_turn_id = input
            .trace_turn_id
            .clone()
            .unwrap_or_else(|| TurnId::from(opts.execution_scope_id()));
        input.trace_turn_id = Some(trace_turn_id.clone());
        // Store-backed new turns acquire and admit the execution lane before
        // the acceptance effect writes mutable session payload (ADR 0077).
        *session_execution_lease = self.claim_session_execution_lease().await?;
        // Acceptance is journaled, not written directly: it happens before the
        // turn runs, which puts it inside a durable engine's replay window, and
        // a replayed handler must re-derive this admission rather than mint a
        // second one (ADR 0069 §6).
        let accepted = opts
            .scoped_effect_controller()
            .controller()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    super::causal::turn_acceptance_effect_invocation(
                        &self.state.session_id,
                        &trace_turn_id,
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        self.state.turn_index + 1,
                    ),
                    crate::RuntimeEffectCommand::AcceptTurnInput {
                        draft: Box::new(crate::PendingTurnInputDraft::new(
                            self.state.session_id.clone(),
                            crate::TurnInputIngress::next_turn(),
                            input.durable_projection(),
                        )),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::turn_acceptance(
                    Arc::clone(&store) as Arc<dyn crate::TurnInputStore>
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_accepted_turn_input)
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        let acceptance = crate::TurnInputAcceptanceReceipt::from(&accepted);
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            lash_trace::TraceContext::default()
                .for_session(self.state.session_id.clone())
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                .for_turn_index(self.state.turn_index + 1)
                .for_turn(trace_turn_id.clone()),
            lash_trace::TraceEvent::Custom {
                name: "turn_input.accepted".to_string(),
                payload: serde_json::json!({ "input_id": &accepted.input_id }),
            },
            self.host.core.clock.as_ref(),
        );

        let drive = {
            let fence = session_execution_lease
                .as_ref()
                .map(SessionExecutionLeaseGuard::fence)
                .expect("a store-backed turn acquires its execution lease before acceptance");
            let mut input_claim = store
                .claim_next_turn_inputs(
                    &self.state.session_id,
                    &fence,
                    &self.runtime_lease_owner,
                    MAX_CLAIMED_TURN_INPUTS,
                )
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            // Same FIG-1573 backstop the queued drain runs: a lane holder
            // that finds its own fresh acceptance unclaimable is looking at
            // rows a dead turn pinned. Repair them, then claim once more in
            // this same call.
            if input_claim.is_none()
                && self
                    .defer_orphaned_turn_inputs_before_drain(
                        &store,
                        &fence,
                        &TurnId::from(opts.execution_scope_id()),
                    )
                    .await
                    > 0
            {
                input_claim = store
                    .claim_next_turn_inputs(
                        &self.state.session_id,
                        &fence,
                        &self.runtime_lease_owner,
                        MAX_CLAIMED_TURN_INPUTS,
                    )
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
            }
            let claimed_own_row = match input_claim {
                Some(claim)
                    if claim
                        .inputs
                        .iter()
                        .any(|pending| pending.input_id == accepted.input_id) =>
                {
                    Some(claim)
                }
                // A claim that reached rows but not this turn's own
                // acceptance is claim-pinning up to
                // `MAX_CLAIMED_TURN_INPUTS` rows this caller will never
                // drive. Hand it straight back instead of dropping it and
                // leaving those rows stalled until the lane generation
                // advances far enough for the FIG-1573 backstop to repair
                // them.
                Some(foreign_claim) => {
                    crate::trace::emit_trace(
                        &self.host.core.tracing.trace_sink,
                        &self.host.core.tracing.trace_context,
                        lash_trace::TraceContext::default()
                            .for_session(self.state.session_id.clone())
                            // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                            .for_turn_index(self.state.turn_index + 1)
                            .for_turn(trace_turn_id.clone()),
                        lash_trace::TraceEvent::Custom {
                            name: "turn_input.claim_abandoned".to_string(),
                            payload: serde_json::json!({
                                "claim_id": &foreign_claim.claim_id,
                                "accepted_input_id": &accepted.input_id,
                                "input_ids": foreign_claim
                                    .inputs
                                    .iter()
                                    .map(|input| input.input_id.clone())
                                    .collect::<Vec<_>>(),
                                "reason": "claim_missed_own_acceptance",
                            }),
                        },
                        self.host.core.clock.as_ref(),
                    );
                    if let Err(abandon_err) = store
                        .abandon_turn_input_claims(std::slice::from_ref(&foreign_claim))
                        .await
                    {
                        tracing::warn!(
                            error = %abandon_err,
                            claim_id = %foreign_claim.claim_id,
                            claim_count = foreign_claim.inputs.len(),
                            "failed to abandon a turn-input claim that missed its own acceptance"
                        );
                    }
                    None
                }
                None => None,
            };
            match claimed_own_row {
                Some(input_claim) => {
                    crate::trace::emit_trace(
                        &self.host.core.tracing.trace_sink,
                        &self.host.core.tracing.trace_context,
                        lash_trace::TraceContext::default()
                            .for_session(self.state.session_id.clone())
                            // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                            .for_turn_index(self.state.turn_index + 1)
                            .for_turn(trace_turn_id.clone()),
                        lash_trace::TraceEvent::Custom {
                            name: "turn_input.claimed".to_string(),
                            payload: serde_json::json!({
                                "claim_id": &input_claim.claim_id,
                                "input_ids": input_claim
                                    .inputs
                                    .iter()
                                    .map(|input| input.input_id.clone())
                                    .collect::<Vec<_>>(),
                            }),
                        },
                        self.host.core.clock.as_ref(),
                    );
                    super::turn_input_ingress::TurnInputDrive::Claimed(input_claim)
                }
                None => {
                    // Probe the row before deciding. Cancellation is a
                    // no-op on every state except an open one, so the same
                    // call both withdraws a withdrawable acceptance and
                    // reports why it could not.
                    let outcome = match store
                        .cancel_pending_turn_input(&self.state.session_id, &accepted.input_id)
                        .await
                    {
                        Ok(outcome) => outcome,
                        // A store fault is not a verdict on the row: it
                        // leaves the acceptance open and undriven, so it
                        // must surface as itself rather than as one of the
                        // dispositions below, each of which tells the
                        // caller something the probe never established.
                        Err(err) => {
                            if let Some(lease) = session_execution_lease.as_ref() {
                                let _ = lease.release_if_live().await;
                            }
                            return Err(super::runtime_error_from_store_commit(err));
                        }
                    };
                    // Intent-fulfilled-iff-result-exists (ADR 0069 §6): a
                    // replayed acceptance whose turn already committed is
                    // settled, not claimable. That is a redrive, not a
                    // contended lane - the turn re-derives the same commit
                    // and the head CAS recognises it - so it drops to the
                    // unclaimed regime like any other driver that could not
                    // fence the row it accepted. One settlement path, two
                    // regimes.
                    if let crate::PendingTurnInputCancelOutcome::AlreadyCompleted(settled) =
                        &outcome
                    {
                        // The redrive must re-derive the first execution's
                        // *turn*, not just its own row. A first execution
                        // that took the lane absorbed every earlier claimed
                        // row into one turn, and a redrive naming fewer rows
                        // materializes different words, hashes differently,
                        // and is refused as a commit conflict instead of
                        // replaying the receipt. So rebuild the settled row
                        // set from the durable applications the first
                        // execution wrote.
                        let unclaimed = self
                            .settled_turn_input_redrive_set(store.as_ref(), settled)
                            .await?;
                        super::turn_input_ingress::TurnInputDrive::Unclaimed(unclaimed)
                    } else if matches!(outcome, crate::PendingTurnInputCancelOutcome::NotFound) {
                        // The journaled acceptance names an identity, not a
                        // write (ADR 0069 §6): a replayed acceptance hands
                        // back the identity the first execution minted
                        // without touching the store, so a substrate that
                        // does not already hold the row has an unfulfilled
                        // intent. Re-admit it under the *same* identity -
                        // that admits this turn again, never a second one -
                        // and drive it unclaimed, which is the same
                        // settlement path the advisory-lane branch uses.
                        let readmitted = store
                            .enqueue_pending_turn_input(
                                crate::PendingTurnInputDraft::new(
                                    self.state.session_id.clone(),
                                    crate::TurnInputIngress::next_turn(),
                                    input.durable_projection(),
                                )
                                .with_input_id(accepted.input_id.clone()),
                            )
                            .await
                            .map_err(super::runtime_error_from_store_commit)?;
                        super::turn_input_ingress::TurnInputDrive::Unclaimed(
                            crate::UnclaimedTurnInputs {
                                session_id: self.state.session_id.clone(),
                                inputs: vec![readmitted],
                                applications: Vec::new(),
                            },
                        )
                    } else {
                        // This caller took the lane and its claim still did
                        // not reach its own unsettled row: another driver
                        // holds it, or it sits behind more than
                        // `MAX_CLAIMED_TURN_INPUTS` earlier admissions. It
                        // cannot settle the row unclaimed either - a
                        // claimed row fails that predicate by construction
                        // - so the caller is told which case it hit rather
                        // than left guessing whether a retry double-submits.
                        if let Some(lease) = session_execution_lease.as_ref() {
                            let _ = lease.release_if_live().await;
                        }
                        let withdrawn = matches!(
                            outcome,
                            crate::PendingTurnInputCancelOutcome::Cancelled(_)
                                | crate::PendingTurnInputCancelOutcome::AlreadyCancelled(_)
                        );
                        return Err(RuntimeError::new(
                            RuntimeErrorCode::SessionExecutionLaneBusy,
                            if withdrawn {
                                format!(
                                    "accepted turn input `{}` was not claimable by this \
                                         caller and has been withdrawn; retrying the same turn \
                                         is safe",
                                    accepted.input_id
                                )
                            } else {
                                format!(
                                    "accepted turn input `{}` was claimed by another driver \
                                         before this caller could drive it; its turn completes \
                                         without this caller",
                                    accepted.input_id
                                )
                            },
                        ));
                    }
                }
            }
        };

        // Drive the accepted rows, not the caller's copy: a claim may carry
        // inputs enqueued before this one, and dropping them would settle rows
        // whose content never reached a turn. Live per-turn state that cannot
        // cross the durable boundary is re-attached from the caller's input.
        let mut driven = drive.materialize_turn_input();
        driven.protocol_turn_options = input
            .protocol_turn_options
            .clone()
            .or(driven.protocol_turn_options);
        driven.trace_turn_id = Some(trace_turn_id);
        driven.protocol_extension = input.protocol_extension.clone();
        driven.turn_context = input.turn_context.clone();

        let claim_for_abandon = drive.clone();
        let scoped_effect_controller = opts.scoped_effect_controller();
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Input(driven),
            opts.events_or_noop(),
            opts.turn_events_or_noop(),
            scoped_effect_controller,
            cancel,
            LogicalTurnClaims::new(Vec::new(), vec![drive]),
            session_execution_lease,
            stopwatch,
        ))
        .await;
        if let Err(err) = &result {
            self.abandon_turn_input_claims_after_local_abort(
                err,
                std::slice::from_ref(&claim_for_abandon),
            )
            .await;
        }
        let mut run = self
            .settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await?;
        // Only the physical turn this acceptance admitted carries it. An
        // agent-frame run's follow-on turns were started by the frame switch,
        // not by this admission, and stamping them would report an acceptance
        // that never applied to them.
        if let Some(admitted) = run.turns.first_mut() {
            admitted.turn_input_acceptance = Some(acceptance.clone());
        }
        run.acceptance = Some(acceptance);
        Ok(run)
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
    #[allow(
        clippy::too_many_arguments,
        reason = "this is the published `LashRuntime::stream_prepared_turn` signature in \
                  docs/api-surface.snapshot; folding these into a context struct would be a \
                  public API change, which this ticket forbids"
    )]
    pub async fn stream_prepared_turn(
        &mut self,
        messages: crate::MessageSequence,
        previous_prompt_usage: Option<PromptUsage>,
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
        protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
        turn_context: crate::TurnContext,
        initial_turn_causes: Vec<crate::TurnCause>,
        trace_turn_id: TurnId,
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
        let result = Box::pin(
            self.drive_logical_turn(
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
                    initial_turn_input_claim
                        .into_iter()
                        .map(super::turn_input_ingress::TurnInputDrive::Claimed)
                        .collect(),
                ),
                &mut session_execution_lease,
                stopwatch,
            ),
        )
        .await
        .map(|run| {
            run.into_final_turn()
                .expect("logical turn always contains a terminal physical turn")
        });
        self.settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await
    }
}
