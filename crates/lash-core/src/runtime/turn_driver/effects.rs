use super::*;

trait ClaimRows {
    type Row;

    fn rows(&self) -> &[Self::Row];
    fn row_key(row: &Self::Row) -> &str;
    fn retain_rows(&mut self, overlapping: &std::collections::HashSet<String>);
}

impl ClaimRows for crate::QueuedWorkClaimData {
    type Row = crate::QueuedWorkBatch;

    fn rows(&self) -> &[Self::Row] {
        &self.batches
    }

    fn row_key(row: &Self::Row) -> &str {
        &row.batch_id
    }

    fn retain_rows(&mut self, overlapping: &std::collections::HashSet<String>) {
        self.batches
            .retain(|batch| !overlapping.contains(Self::row_key(batch)));
    }
}

impl ClaimRows for crate::TurnInputClaimData {
    type Row = crate::PendingTurnInput;

    fn rows(&self) -> &[Self::Row] {
        &self.inputs
    }

    fn row_key(row: &Self::Row) -> &str {
        &row.input_id
    }

    fn retain_rows(&mut self, overlapping: &std::collections::HashSet<String>) {
        self.inputs
            .retain(|input| !overlapping.contains(Self::row_key(input)));
        self.applications
            .retain(|application| !overlapping.contains(application.input_id.as_str()));
    }
}

fn compare_claim_authority<C>(
    left: &crate::WorkClaim<C>,
    right: &crate::WorkClaim<C>,
) -> std::cmp::Ordering {
    (left.session_lease_generation, left.fencing_token)
        .cmp(&(right.session_lease_generation, right.fencing_token))
}

fn merge_pending_claim_authority<C: ClaimRows>(
    pending_claims: &mut [&mut crate::WorkClaim<C>],
    incoming: &mut crate::WorkClaim<C>,
    row_kind: &str,
    mut on_lower_authority: impl FnMut(
        &mut crate::WorkClaim<C>,
        &mut crate::WorkClaim<C>,
        &std::collections::HashSet<String>,
    ),
) -> Result<(), RuntimeError> {
    for pending in pending_claims.iter_mut() {
        let overlapping = pending
            .data
            .rows()
            .iter()
            .filter(|pending_row| {
                incoming
                    .data
                    .rows()
                    .iter()
                    .any(|incoming_row| C::row_key(incoming_row) == C::row_key(pending_row))
            })
            .map(|row| C::row_key(row).to_string())
            .collect::<std::collections::HashSet<_>>();
        if overlapping.is_empty() {
            continue;
        }

        match compare_claim_authority(*pending, incoming) {
            std::cmp::Ordering::Less => {
                on_lower_authority(pending, incoming, &overlapping);
                pending.data.retain_rows(&overlapping);
            }
            std::cmp::Ordering::Greater => incoming.data.retain_rows(&overlapping),
            std::cmp::Ordering::Equal
                if pending.claim_id == incoming.claim_id
                    && pending.lease_token == incoming.lease_token =>
            {
                incoming.data.retain_rows(&overlapping);
            }
            std::cmp::Ordering::Equal => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!(
                        "{row_kind} rows {overlapping:?} have conflicting claim authorities `{}` and `{}` at session generation {} and fencing token {}",
                        pending.claim_id,
                        incoming.claim_id,
                        incoming.session_lease_generation,
                        incoming.fencing_token,
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Whether an incoming queued-work claim names a batch this turn already
/// drives, which marks it as a superseded replay rather than new work.
fn claim_shares_queued_batches(
    pending_claims: &[crate::QueuedWorkClaim],
    incoming: &crate::QueuedWorkClaim,
) -> bool {
    pending_claims.iter().any(|pending| {
        pending.batches.iter().any(|pending_batch| {
            incoming
                .batches
                .iter()
                .any(|incoming_batch| incoming_batch.batch_id == pending_batch.batch_id)
        })
    })
}

/// Whether an incoming turn-input claim names a row this turn already drives.
fn claim_shares_turn_input_rows(
    pending_drives: &[crate::TurnInputClaim],
    incoming: &crate::TurnInputClaim,
) -> bool {
    pending_drives.iter().any(|drive| {
        drive.inputs.iter().any(|pending_input| {
            incoming
                .inputs
                .iter()
                .any(|incoming_input| incoming_input.input_id == pending_input.input_id)
        })
    })
}

fn merge_pending_queue_claim_authority(
    pending_claims: &mut Vec<crate::QueuedWorkClaim>,
    mut incoming: crate::QueuedWorkClaim,
) -> Result<(), RuntimeError> {
    let mut pending_refs = pending_claims.iter_mut().collect::<Vec<_>>();
    merge_pending_claim_authority(
        &mut pending_refs,
        &mut incoming,
        "queued-work",
        |_, _, _| {},
    )?;
    drop(pending_refs);
    pending_claims.retain(|claim| !claim.batches.is_empty());
    if !incoming.batches.is_empty() {
        pending_claims.push(incoming);
    }
    Ok(())
}

/// Reconcile a checkpoint's fresh claim against the claims this turn already
/// holds.
///
/// Every row a turn drives is held under a claim, including the journaled
/// initial drive set (ADR 0069 §6), so every pending claim takes part.
fn merge_pending_turn_input_claim_authority(
    pending_drives: &mut Vec<crate::TurnInputClaim>,
    incoming: &mut crate::TurnInputClaim,
) -> Result<std::collections::HashSet<String>, RuntimeError> {
    let mut already_delivered = std::collections::HashSet::new();
    let mut pending_claims = pending_drives.iter_mut().collect::<Vec<_>>();
    merge_pending_claim_authority(
        &mut pending_claims,
        incoming,
        "turn-input",
        |pending, incoming, overlapping| {
            already_delivered.extend(overlapping.iter().cloned());
            for application in pending
                .applications
                .iter()
                .filter(|application| overlapping.contains(application.input_id.as_str()))
            {
                if !incoming
                    .applications
                    .iter()
                    .any(|existing| existing.input_id == application.input_id)
                {
                    incoming.applications.push(application.clone());
                }
            }
        },
    )?;
    drop(pending_claims);
    pending_drives.retain(|drive| !drive.inputs.is_empty());
    Ok(already_delivered)
}

fn merge_pending_checkpoint_turn_input_claim(
    pending: &mut Option<crate::TurnInputClaim>,
    incoming: crate::TurnInputClaim,
) -> Result<(), RuntimeError> {
    match pending.as_ref() {
        None => *pending = Some(incoming),
        Some(existing) if existing.claim_id == incoming.claim_id => {}
        Some(existing) => {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                format!(
                    "checkpoint replay returned turn-input claim `{}` while `{}` is pending",
                    incoming.claim_id, existing.claim_id
                ),
            ));
        }
    }
    Ok(())
}

/// The claims a turn holds, as the checkpoint fold updates them.
struct TurnClaimSlots<'a> {
    pending_queue_claims: &'a mut Vec<crate::QueuedWorkClaim>,
    pending_turn_input_claims: &'a mut Vec<crate::TurnInputClaim>,
    pending_checkpoint_turn_input_claim: &'a mut Option<crate::TurnInputClaim>,
    withheld_terminal_work: &'a mut crate::runtime::logical_turn::WithheldTerminalWork,
}

/// Folds a checkpoint's recorded claim set into the turn's claims by the rule
/// the step body applied when it claimed them: at a terminal checkpoint, a
/// claim this turn does not already drive is withheld work, not this turn's to
/// settle.
fn absorb_checkpoint_claims(
    slots: TurnClaimSlots<'_>,
    checkpoint: CheckpointKind,
    queued_work_claims: Vec<crate::QueuedWorkClaim>,
    turn_input_claim: Option<crate::TurnInputClaim>,
) -> Result<(), RuntimeError> {
    let TurnClaimSlots {
        pending_queue_claims,
        pending_turn_input_claims,
        pending_checkpoint_turn_input_claim,
        withheld_terminal_work,
    } = slots;
    let withholds_claimed_work = matches!(checkpoint, CheckpointKind::BeforeCompletion);
    for claim in queued_work_claims {
        if withholds_claimed_work && !claim_shares_queued_batches(pending_queue_claims, &claim) {
            merge_pending_queue_claim_authority(&mut withheld_terminal_work.queued, claim)?;
        } else {
            merge_pending_queue_claim_authority(pending_queue_claims, claim)?;
        }
    }
    if let Some(mut claim) = turn_input_claim {
        if withholds_claimed_work
            && !claim_shares_turn_input_rows(pending_turn_input_claims, &claim)
            && !pending_checkpoint_turn_input_claim
                .as_ref()
                .is_some_and(|pending| {
                    pending.inputs.iter().any(|input| {
                        claim
                            .inputs
                            .iter()
                            .any(|incoming| incoming.input_id == input.input_id)
                    })
                })
        {
            merge_pending_turn_input_claim_authority(
                &mut withheld_terminal_work.turn_inputs,
                &mut claim,
            )?;
            if !claim.inputs.is_empty() {
                withheld_terminal_work.turn_inputs.push(claim);
            }
        } else {
            // A replayed checkpoint outcome can re-deliver a claim this turn
            // already drives — the withheld claim a follow-on turn was admitted
            // with is carried by the journaled claim set. Reconcile it against
            // the resident drives first so the same authority registers exactly
            // one drive; only rows no drive covers are new work for the pending
            // checkpoint slot.
            merge_pending_turn_input_claim_authority(pending_turn_input_claims, &mut claim)?;
            if !claim.inputs.is_empty() {
                merge_pending_checkpoint_turn_input_claim(
                    pending_checkpoint_turn_input_claim,
                    claim,
                )?;
            }
        }
    }
    Ok(())
}

impl RuntimeTurnDriver<'_> {
    fn merge_pending_queue_claim_authority(
        &mut self,
        claim: crate::QueuedWorkClaim,
    ) -> Result<(), RuntimeError> {
        merge_pending_queue_claim_authority(&mut self.pending_queue_claims, claim)
    }

    pub(in crate::runtime) async fn execute_checkpoint_locally(
        &mut self,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        checkpoint: CheckpointKind,
        event_tx: &TurnObserver,
    ) -> RuntimeEffectOutcome {
        let result = self
            .run_checkpoint(messages, protocol_iteration, checkpoint, event_tx)
            .await
            .map_err(RuntimeEffectControllerError::from);
        RuntimeEffectOutcome::Checkpoint {
            result,
            claims: Box::new(crate::runtime::effect::CheckpointClaimSet {
                // A checkpoint outcome is a self-contained authority snapshot.
                // Replay must never reconstruct it from mutations to the
                // driver's resident claim set. Work withheld from a terminal
                // checkpoint (FIG-3157) is part of that authority: replay
                // routes it back by the same rule that withheld it, so the
                // journal needs no new shape to carry it.
                queued_work_claims: self
                    .pending_queue_claims
                    .iter()
                    .chain(self.withheld_terminal_work.queued.iter())
                    .cloned()
                    .collect(),
                turn_input_claim: self
                    .pending_checkpoint_turn_input_claim
                    .clone()
                    .or_else(|| self.withheld_terminal_work.turn_inputs.last().cloned()),
                incorporation: self.opener_state.ledger_snapshot(),
            }),
        }
    }

    pub(super) async fn invoke_turn_checkpoint_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        checkpoint: CheckpointKind,
        event_tx: &TurnObserver,
    ) -> Result<crate::CheckpointDelivery, RuntimeEffectControllerError> {
        let invocation = self.turn_effect_invocation(machine, id, RuntimeEffectKind::Checkpoint)?;
        let (result, claims) = self
            .execute_typed_turn_effect(
                machine,
                event_tx,
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::Checkpoint { checkpoint },
                ),
                RuntimeEffectOutcome::into_checkpoint,
            )
            .await?;
        let crate::runtime::effect::CheckpointClaimSet {
            queued_work_claims,
            turn_input_claim,
            incorporation,
        } = claims;
        // A replayed checkpoint is the authority for everything the turn
        // incorporated and enqueued before it. The cells before it re-run on
        // replay (ADR 0103) and re-incorporate the same settlements, which
        // refills the checkpoint message buffer the live checkpoint drained.
        // The recorded delivery already carries those messages, so the refill
        // is discarded here; after a live checkpoint the buffer is already
        // empty and this drains nothing.
        self.checkpoint_messages.drain();
        self.opener_state.absorb_ledger(incorporation);
        // The recorded claim set is the only way the checkpoint's claims
        // reach this driver: the step body ran on a copy of it. It is folded
        // in before the result is read, so a checkpoint that claimed work and
        // then failed hands that work to the failure path on the live pass
        // and on every replay alike.
        self.absorb_checkpoint_claims(checkpoint, queued_work_claims, turn_input_claim)
            .map_err(RuntimeEffectControllerError::from)?;
        // A failed checkpoint is part of the checkpoint's own recorded
        // outcome: the journal holds it, and every redrive replays it. It is
        // therefore an outcome whatever its code (FIG-3528, FIG-3575), never
        // an abort a redrive would reproduce forever.
        result.map_err(RuntimeEffectControllerError::into_journaled)
    }

    fn absorb_checkpoint_claims(
        &mut self,
        checkpoint: CheckpointKind,
        queued_work_claims: Vec<crate::QueuedWorkClaim>,
        turn_input_claim: Option<crate::TurnInputClaim>,
    ) -> Result<(), RuntimeError> {
        absorb_checkpoint_claims(
            TurnClaimSlots {
                pending_queue_claims: &mut self.pending_queue_claims,
                pending_turn_input_claims: &mut self.pending_turn_input_claims,
                pending_checkpoint_turn_input_claim: &mut self.pending_checkpoint_turn_input_claim,
                withheld_terminal_work: &mut self.withheld_terminal_work,
            },
            checkpoint,
            queued_work_claims,
            turn_input_claim,
        )
    }

    /// Phase 2 of [`RuntimeTurnDriver::invoke_turn_llm_effect`].
    pub(super) async fn invoke_assistant_response_hooks_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        response: LlmResponse,
        stream_hook_states: Vec<crate::runtime::AssistantStreamHookState>,
        event_tx: &TurnObserver,
    ) -> Result<LlmResponse, RuntimeEffectControllerError> {
        // Rebuilt rather than threaded through: phase 1's invocation is a pure
        // function of the same turn identity, so this is the identical parent
        // and the causal edge survives a redrive that only runs phase 2.
        let phase_one = self.turn_effect_invocation(machine, id, RuntimeEffectKind::LlmCall)?;
        let invocation = crate::runtime::causal::turn_phase_effect_invocation(
            self.scoped_effect_controller.execution_scope(),
            &phase_one,
            id,
            RuntimeEffectKind::AssistantResponseHooks,
        );
        let (response, events) = self
            .execute_typed_turn_effect(
                machine,
                event_tx,
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::AssistantResponseHooks {
                        response: Box::new(response),
                        stream_hook_states,
                    },
                ),
                RuntimeEffectOutcome::into_assistant_response_hooks,
            )
            .await?;
        // Emitted from the decoded outcome, so a replayed phase 2 serves the
        // recorded events rather than re-running the hooks that produced them.
        for emitted in events {
            for event in
                crate::plugin::plugin_runtime_session_events(&emitted.plugin_id, emitted.events)
            {
                event_tx.session(event);
            }
        }
        Ok(response)
    }

    pub(super) async fn invoke_turn_execution_environment_sync_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        event_tx: &TurnObserver,
    ) -> Result<crate::runtime::effect::ServedExecutionEnvironmentSync, RuntimeEffectControllerError>
    {
        let invocation =
            self.turn_effect_invocation(machine, id, RuntimeEffectKind::SyncExecutionEnvironment)?;
        self.execute_typed_turn_effect(
            machine,
            event_tx,
            RuntimeEffectEnvelope::new(invocation, RuntimeEffectCommand::SyncExecutionEnvironment),
            RuntimeEffectOutcome::into_sync_execution_environment,
        )
        .await
    }

    pub(super) async fn invoke_turn_exec_effect(
        &mut self,
        machine: &mut TurnMachine,
        invocation: crate::RuntimeEffectInvocation,
        language: String,
        code: String,
        event_tx: &TurnObserver,
    ) -> Result<Result<crate::ExecResponse, crate::ExecCodeFailure>, RuntimeEffectControllerError>
    {
        self.execute_typed_turn_effect(
            machine,
            event_tx,
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::ExecCode { language, code },
            ),
            RuntimeEffectOutcome::into_exec_code,
        )
        .await
    }

    pub(in crate::runtime) async fn run_checkpoint(
        &mut self,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        checkpoint: CheckpointKind,
        event_tx: &TurnObserver,
    ) -> Result<crate::CheckpointDelivery, RuntimeError> {
        let mut committed = self.checkpoint_messages.drain();
        let mut transient_messages = Vec::new();
        let mut committed_user_messages = Vec::new();
        let mut turn_causes = Vec::new();
        let (turn_input_claim, queue_claim) = if let Some(store) = self.session.history_store() {
            if let Some(session_execution_lease) = self.session_execution_lease.as_ref() {
                let mut claim_policy = self
                    .host
                    .core
                    .durability
                    .queued_work_batching
                    .claim_policy(self.policy.context_window_tokens());
                claim_policy.max_rows = self
                    .turn_context
                    .checkpoint_queued_work_limit(claim_policy.max_rows);
                match store
                    .claim_checkpoint_work(
                        &self.session_id,
                        session_execution_lease,
                        &self.runtime_lease_owner,
                        &self.turn_id,
                        checkpoint,
                        64,
                        claim_policy,
                    )
                    .await
                {
                    Ok(claims) => claims,
                    Err(err @ crate::StoreError::SessionExecutionLeaseExpired { .. }) => {
                        tracing::warn!(
                            session_id = %self.session_id,
                            turn_id = %self.turn_id,
                            event = "session_execution_lease.checkpoint_advisory",
                            cause = %err,
                            "session execution lease expired; skipping advisory checkpoint claims"
                        );
                        self.session_execution_lease = None;
                        (None, None)
                    }
                    Err(err) => return Err(crate::runtime::runtime_error_from_store_commit(err)),
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        debug_assert!(
            self.pending_checkpoint_turn_input_claim.is_none(),
            "checkpoint claims must be resolved before another checkpoint runs"
        );
        // FIG-3157: a terminal finish ends the turn, so work claimed at this
        // boundary never extends it. The claim is withheld from the delivery
        // and starts a follow-on turn inside the same logical run instead.
        // The boundary that claimed it is still the boundary it reports.
        let withholds_claimed_work = matches!(checkpoint, CheckpointKind::BeforeCompletion);
        if let Some(mut claim) = turn_input_claim {
            let already_delivered = merge_pending_turn_input_claim_authority(
                &mut self.pending_turn_input_claims,
                &mut claim,
            )?;
            // A claim that re-delivers rows this turn already committed is a
            // superseded replay of this turn's own work, not new input: it
            // settles here rather than starting a turn of its own.
            if withholds_claimed_work && already_delivered.is_empty() && !claim.inputs.is_empty() {
                merge_pending_turn_input_claim_authority(
                    &mut self.withheld_terminal_work.turn_inputs,
                    &mut claim,
                )?;
                // The row was accepted at this boundary; only the turn that
                // renders it moves. Applications are recorded by that turn.
                let accepted_turn_inputs = claim.accepted_turn_inputs();
                self.withheld_terminal_work.turn_inputs.push(claim);
                if !accepted_turn_inputs.is_empty() {
                    event_tx.session(SessionStreamEvent::InjectedTurnInputAccepted {
                        inputs: accepted_turn_inputs,
                        checkpoint,
                    });
                }
            } else {
                let mut delivery_claim = claim.clone();
                delivery_claim
                    .inputs
                    .retain(|input| !already_delivered.contains(input.input_id.as_str()));
                self.pending_checkpoint_turn_input_claim = Some(claim);
                let materialized = delivery_claim
                    .materialize_checkpoint_turn_input(
                        &self.turn_id,
                        self.host.core.durability.attachment_store.as_ref(),
                        self.host.core.attachment_source_policy.as_ref(),
                    )
                    .await
                    .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err))?;
                committed_user_messages.extend(materialized.messages);
                turn_causes.extend(materialized.turn_causes);
            }
        }
        if let Some(claim) = queue_claim {
            let materialized = claim.materialize_queued_checkpoint_work();
            send_queued_work_started_event(
                event_tx,
                crate::QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                &claim,
                materialized.turn_causes.clone(),
            );
            self.emit_trace(
                protocol_iteration,
                lash_trace::TraceEvent::Custom {
                    name: "queued_work.claimed".to_string(),
                    payload: queued_work_trace_payload(
                        crate::QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                        &claim,
                        &materialized.turn_causes,
                    ),
                },
            );
            if withholds_claimed_work
                && !claim_shares_queued_batches(&self.pending_queue_claims, &claim)
            {
                merge_pending_queue_claim_authority(
                    &mut self.withheld_terminal_work.queued,
                    claim,
                )?;
            } else {
                turn_causes.extend(materialized.turn_causes);
                self.merge_pending_queue_claim_authority(claim)?;
            }
        }
        let plugins = Arc::clone(self.session.plugins());
        let applied = plugins
            .apply_checkpoint(CheckpointHookContext {
                session_id: self.session_id.clone(),
                checkpoint,
                state: self
                    .checkpoint_state_view(messages, protocol_iteration)
                    .map_err(|error| {
                        RuntimeError::new(RuntimeErrorCode::PluginCheckpoint, error.to_string())
                    })?,
                sessions: self.session_services.state_service(),
                session_lifecycle: self.session_services.lifecycle_service(),
                session_graph: self.session_services.graph_service(),
            })
            .await
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginCheckpoint))?;
        committed.extend(applied.messages);
        emit_session_events(event_tx, applied.events);
        if let Some(abort) = applied.abort {
            // A plugin's abort code is plugin-authored vocabulary: it lands
            // in `ForeignCode` verbatim (namespace included) and is never
            // re-parsed into a Lash `RuntimeErrorCode` arm.
            return Err(RuntimeError::foreign(
                abort.code.namespaced(),
                crate::TurnFailureCause::Outcome,
                abort.message,
            ));
        }

        normalize_plugin_message_attachments(
            &mut committed,
            self.host.core.durability.attachment_store.as_ref(),
            self.host.core.attachment_source_policy.as_ref(),
        )
        .await?;
        normalize_plugin_message_attachments(
            &mut transient_messages,
            self.host.core.durability.attachment_store.as_ref(),
            self.host.core.attachment_source_policy.as_ref(),
        )
        .await?;

        if !committed.is_empty() {
            event_tx.session(SessionStreamEvent::InjectedMessagesCommitted {
                messages: committed.clone(),
                checkpoint,
            });
        }

        Ok(crate::CheckpointDelivery {
            committed_user_messages,
            messages: committed,
            transient_messages,
            turn_causes,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "foreground code execution carries explicit turn and replay context"
    )]
    pub(in crate::runtime) async fn run_exec_code(
        &self,
        language: String,
        code: &str,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        cell_replay_grammar: Option<u32>,
        invocation: crate::RuntimeInvocation,
        event_tx: &TurnObserver,
    ) -> Result<
        Result<crate::ExecResponse, crate::ExecCodeFailure>,
        crate::RuntimeEffectControllerError,
    > {
        let code_executor = self.session.plugins().code_executor();
        // A code executor that keys its cells' nested effects by a versioned
        // replay-key grammar runs a cell only under the grammar its
        // iteration's journaled sync names (FIG-3586): the refusal is the
        // executor's, before the cell runs.
        if let Some(executor) = &code_executor {
            executor.admit_replay_key_grammar(cell_replay_grammar)?;
        }
        let (session_event_tx, mut session_event_rx) = mpsc::channel::<SessionStreamEvent>(100);
        let (turn_event_tx, mut turn_event_rx) = mpsc::channel::<TurnActivity>(100);
        let relay_tx = event_tx.clone();
        let relay_handle = crate::task::spawn(async move {
            let mut session_closed = false;
            let mut turn_closed = false;
            while !(session_closed && turn_closed) {
                tokio::select! {
                    biased;
                    maybe_event = session_event_rx.recv(), if !session_closed => {
                        let Some(event) = maybe_event else {
                            session_closed = true;
                            continue;
                        };
                        relay_tx.session(event);
                    }
                    maybe_turn_event = turn_event_rx.recv(), if !turn_closed => {
                        let Some(event) = maybe_turn_event else {
                            turn_closed = true;
                            continue;
                        };
                        relay_tx.publish(RuntimeStreamEvent::Turn(event));
                    }
                }
            }
        });
        let read_view = self
            .checkpoint_state_view(messages, protocol_iteration)
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(crate::PluginError::Session(
                    error.to_string(),
                ))
            })?;
        let chronological_projection = read_view.shared_chronological_projection();
        let code_block_graph_key = foreground_exec_graph_key(&invocation);
        let context = self
            .execution_context(session_event_tx.clone(), event_tx, chronological_projection)
            .map_err(crate::RuntimeEffectControllerError::from)?
            .with_turn_event_sender(turn_event_tx.clone())
            .with_tracing(self.execution_tracing(protocol_iteration))
            .with_code_block_graph_key(code_block_graph_key);
        let context = context.with_parent_invocation(invocation);
        let result = match code_executor {
            Some(code_executor) => code_executor
                .execute_code(
                    context.clone(),
                    crate::ExecRequest {
                        language,
                        code: code.to_string(),
                    },
                )
                .await
                .map_err(|e| e.to_exec_code_failure()),
            None => Err(crate::SessionError::CodeExecutionUnavailable.to_exec_code_failure()),
        };
        let nested_effect_error = context.take_nested_effect_error();
        drop(context);
        drop(session_event_tx);
        drop(turn_event_tx);
        let _ = relay_handle.await;
        match nested_effect_error {
            Some(error) => Err(error),
            None => Ok(result),
        }
    }
}

async fn normalize_plugin_message_attachments(
    messages: &mut [crate::PluginMessage],
    attachment_store: &crate::SessionAttachmentStore,
    policy: &dyn crate::AttachmentSourcePolicy,
) -> Result<(), RuntimeError> {
    for message in messages {
        for part in &mut message.parts {
            for source in part.attachment_sources_mut() {
                normalize_plugin_attachment_source(source, attachment_store, policy).await?;
            }
        }
    }
    Ok(())
}

async fn normalize_plugin_attachment_source(
    source: &mut crate::AttachmentSource,
    attachment_store: &crate::SessionAttachmentStore,
    policy: &dyn crate::AttachmentSourcePolicy,
) -> Result<(), RuntimeError> {
    policy
        .authorize(&crate::AttachmentProducer::Host, source)
        .map_err(|err| RuntimeError::new(RuntimeErrorCode::PluginCheckpoint, err.to_string()))?;
    if let crate::AttachmentSource::Inline { media_type, bytes } = source {
        let attachment_ref = attachment_store
            .put(
                bytes.clone(),
                crate::AttachmentCreateMeta::new(media_type.clone(), None, None),
            )
            .await
            .map_err(|err| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!("failed to store inline checkpoint attachment: {err}"),
                )
            })?;
        *source = crate::AttachmentSource::stored(attachment_ref);
    }
    Ok(())
}

#[cfg(test)]
mod claim_authority_tests {
    use super::*;

    fn coalesced_batch(batch_id: &str, enqueue_seq: u64) -> crate::QueuedWorkBatch {
        crate::QueuedWorkBatch {
            batch_id: batch_id.to_string().into(),
            session_id: SessionId::from("fig905"),
            enqueue_seq,
            source_key: Some(format!("fig905:{batch_id}")),
            delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
            kind: crate::QueuedWorkKind::Turn,
            authority: crate::QueuedWorkAuthority::new("fig905"),
            merge_key: Some("fig905".to_string()),
            available_at_ms: 0,
            enqueued_at_ms: 0,
            items: Vec::new(),
        }
    }

    fn claim(
        claim_id: &str,
        generation: u64,
        fencing_token: u64,
        batches: &[(&str, u64)],
    ) -> crate::QueuedWorkClaim {
        crate::QueuedWorkClaim {
            session_id: SessionId::from("fig905"),
            claim_id: claim_id.to_string(),
            owner: crate::LeaseOwnerIdentity::opaque("fig905", claim_id),
            lease_token: format!("token:{claim_id}"),
            fencing_token,
            session_lease_generation: generation,
            data: crate::QueuedWorkClaimData {
                batches: batches
                    .iter()
                    .map(|(batch_id, enqueue_seq)| coalesced_batch(batch_id, *enqueue_seq))
                    .collect(),
                abandon_restore_claim_id: None,
                abandon_restore_claim_token: None,
            },
        }
    }

    fn authorities(claims: &[crate::QueuedWorkClaim]) -> Vec<(&str, &str)> {
        let mut rows = claims
            .iter()
            .flat_map(|claim| {
                claim
                    .batches
                    .iter()
                    .map(move |batch| (batch.batch_id.as_str(), claim.claim_id.as_str()))
            })
            .collect::<Vec<_>>();
        rows.sort_unstable();
        rows
    }

    fn pending_turn_input(input_id: &str) -> crate::PendingTurnInput {
        crate::PendingTurnInput {
            input_id: input_id.to_string().into(),
            session_id: SessionId::from("fig905"),
            enqueue_seq: 1,
            source_key: None,
            state: crate::TurnInputState::Accepted(crate::ActiveTurnIngress {
                turn_id: "fig905-turn".into(),
                min_boundary: crate::TurnInputCheckpointBoundary::AfterWork,
            }),
            enqueued_at_ms: 0,
            input: crate::TurnInput::text("fig905 input"),
        }
    }

    fn turn_input_claim(
        claim_id: &str,
        generation: u64,
        fencing_token: u64,
        input_ids: &[&str],
    ) -> crate::TurnInputClaim {
        crate::TurnInputClaim {
            session_id: SessionId::from("fig905"),
            claim_id: claim_id.to_string(),
            owner: crate::LeaseOwnerIdentity::opaque("fig905", claim_id),
            lease_token: format!("token:{claim_id}"),
            fencing_token,
            session_lease_generation: generation,
            data: crate::TurnInputClaimData {
                mode: crate::TurnInputClaimMode::ActiveTurn {
                    turn_id: crate::TurnId::from("fig905-turn"),
                    checkpoint: crate::CheckpointKind::AfterWork,
                },
                inputs: input_ids
                    .iter()
                    .map(|input_id| pending_turn_input(input_id))
                    .collect(),
                applications: Vec::new(),
            },
        }
    }

    #[test]
    fn one_overlap_keeps_the_successor_in_the_complete_checkpoint_claim_set() {
        let mut pending = vec![claim("predecessor", 1, 1, &[("a", 1)])];
        merge_pending_queue_claim_authority(&mut pending, claim("successor", 2, 2, &[("a", 1)]))
            .expect("merge one overlapping row");

        assert_eq!(authorities(&pending), vec![("a", "successor")]);
    }

    #[test]
    fn two_coalesced_overlaps_replace_both_rows_without_slice_arithmetic() {
        let mut pending = vec![
            claim("predecessor-a", 1, 1, &[("a", 1)]),
            claim("predecessor-b", 1, 1, &[("b", 2)]),
        ];
        merge_pending_queue_claim_authority(
            &mut pending,
            claim("successor", 2, 2, &[("a", 1), ("b", 2)]),
        )
        .expect("merge the coalesced claim shape");

        assert_eq!(
            authorities(&pending),
            vec![("a", "successor"), ("b", "successor")]
        );
    }

    #[test]
    fn partial_overlap_retains_the_predecessors_non_overlapping_row() {
        let mut pending = vec![claim("predecessor", 1, 1, &[("a", 1), ("b", 2)])];
        merge_pending_queue_claim_authority(&mut pending, claim("successor", 2, 2, &[("a", 1)]))
            .expect("merge a partial overlap");

        assert_eq!(
            authorities(&pending),
            vec![("a", "successor"), ("b", "predecessor")]
        );
    }

    #[test]
    fn restored_older_authority_cannot_replace_a_live_successor() {
        let mut pending = vec![claim("successor", 3, 4, &[("a", 1)])];
        merge_pending_queue_claim_authority(
            &mut pending,
            claim("restored-predecessor", 2, 3, &[("a", 1)]),
        )
        .expect("ignore stale replay authority");

        assert_eq!(authorities(&pending), vec![("a", "successor")]);
    }

    #[test]
    fn equal_queued_work_authority_with_different_claims_is_rejected() {
        let mut pending = vec![claim("first", 2, 3, &[("a", 1)])];
        let error = merge_pending_queue_claim_authority(
            &mut pending,
            claim("conflicting", 2, 3, &[("a", 1)]),
        )
        .expect_err("equal authority must not silently choose a queued-work claim");

        assert_eq!(error.code, RuntimeErrorCode::StoreCommitFailed);
        assert!(error.message.contains("conflicting claim authorities"));
        assert_eq!(authorities(&pending), vec![("a", "first")]);
    }

    #[test]
    fn equal_turn_input_authority_with_different_claims_is_rejected() {
        let mut pending = vec![turn_input_claim("first", 2, 3, &["input-a"])];
        let mut incoming = turn_input_claim("conflicting", 2, 3, &["input-a"]);
        let error = merge_pending_turn_input_claim_authority(&mut pending, &mut incoming)
            .expect_err("equal authority must not silently choose a turn-input claim");

        assert_eq!(error.code, RuntimeErrorCode::StoreCommitFailed);
        assert!(error.message.contains("conflicting claim authorities"));
        assert_eq!(pending[0].claim_id, "first");
    }

    #[test]
    fn lower_turn_input_authority_records_delivery_and_adopts_applications() {
        let mut predecessor = turn_input_claim("predecessor", 1, 1, &["input-a"]);
        predecessor.applications.push(crate::TurnInputApplication {
            input_id: "input-a".into(),
            source_key: None,
            turn_id: crate::TurnId::from("fig905-turn"),
            committed_message_id: "message-a".to_string(),
            checkpoint: Some(crate::CheckpointKind::AfterWork),
        });
        let mut pending = vec![predecessor];
        let mut incoming = turn_input_claim("successor", 2, 2, &["input-a"]);

        let already_delivered =
            merge_pending_turn_input_claim_authority(&mut pending, &mut incoming)
                .expect("merge a lower-authority turn-input claim");

        assert_eq!(
            already_delivered,
            std::collections::HashSet::from(["input-a".to_string()])
        );
        assert!(pending.is_empty());
        assert_eq!(incoming.inputs.len(), 1);
        assert_eq!(incoming.applications.len(), 1);
        assert_eq!(incoming.applications[0].committed_message_id, "message-a");
    }

    #[test]
    fn higher_turn_input_authority_discards_the_incoming_rows_and_applications() {
        let mut pending = vec![turn_input_claim("successor", 2, 2, &["input-a"])];
        let mut incoming = turn_input_claim("predecessor", 1, 1, &["input-a"]);
        incoming.applications.push(crate::TurnInputApplication {
            input_id: "input-a".into(),
            source_key: None,
            turn_id: crate::TurnId::from("fig905-turn"),
            committed_message_id: "message-a".to_string(),
            checkpoint: Some(crate::CheckpointKind::AfterWork),
        });

        let already_delivered =
            merge_pending_turn_input_claim_authority(&mut pending, &mut incoming)
                .expect("merge a higher-authority turn-input claim");

        assert!(already_delivered.is_empty());
        assert_eq!(pending.len(), 1);
        assert!(incoming.inputs.is_empty());
        assert!(incoming.applications.is_empty());
    }

    #[test]
    fn checkpoint_replay_rejects_a_conflicting_pending_turn_input_claim() {
        let mut pending = Some(turn_input_claim("pending", 2, 3, &["input-a"]));
        let error = merge_pending_checkpoint_turn_input_claim(
            &mut pending,
            turn_input_claim("replayed", 1, 2, &["input-a"]),
        )
        .expect_err("replay must not replace a different pending checkpoint claim");

        assert_eq!(error.code, RuntimeErrorCode::StoreCommitFailed);
        assert!(error.message.contains("checkpoint replay returned"));
        assert_eq!(pending.expect("pending claim survives").claim_id, "pending");
    }
}

/// The checkpoint's claims reach the driver through its recorded outcome
/// alone, so a failed terminal checkpoint hands the same claims to its
/// failure path on the live pass and on every replay: cold, on a separate
/// worker, and under perturbed scheduling.
#[cfg(test)]
mod checkpoint_claim_determinism_tests {
    use super::*;
    use crate::engine::testing::{
        DeterminismCheck, FailureCause, LocalEngine, LocalTestCx, ReplayMode, RunMode,
    };
    use lash_sansio::sync::MutexExt;
    use std::future::Future;
    use std::pin::Pin;

    fn queued_claim(claim_id: &str, batch_id: &str) -> crate::QueuedWorkClaim {
        crate::QueuedWorkClaim {
            session_id: SessionId::from("p7"),
            claim_id: claim_id.to_string(),
            owner: crate::LeaseOwnerIdentity::opaque("p7", claim_id),
            lease_token: format!("token:{claim_id}"),
            fencing_token: 1,
            session_lease_generation: 1,
            data: crate::QueuedWorkClaimData {
                batches: vec![crate::QueuedWorkBatch {
                    batch_id: batch_id.to_string().into(),
                    session_id: SessionId::from("p7"),
                    enqueue_seq: 1,
                    source_key: None,
                    delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
                    kind: crate::QueuedWorkKind::Turn,
                    authority: crate::QueuedWorkAuthority::new("p7"),
                    merge_key: None,
                    available_at_ms: 0,
                    enqueued_at_ms: 0,
                    items: Vec::new(),
                }],
                abandon_restore_claim_id: None,
                abandon_restore_claim_token: None,
            },
        }
    }

    /// The step body of a terminal checkpoint that claims `fresh` from the
    /// store and then fails: its outcome is the failure plus the complete
    /// claim set, the admitted claim included.
    fn failed_checkpoint(
        admitted: crate::QueuedWorkClaim,
        fresh: crate::QueuedWorkClaim,
    ) -> RuntimeEffectOutcome {
        RuntimeEffectOutcome::Checkpoint {
            result: Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::PluginCheckpoint,
                "the checkpoint hook refused",
            )),
            claims: Box::new(crate::runtime::effect::CheckpointClaimSet {
                queued_work_claims: vec![admitted, fresh],
                turn_input_claim: None,
                incorporation: Default::default(),
            }),
        }
    }

    fn claim_ids(claims: &[crate::QueuedWorkClaim]) -> Vec<String> {
        claims.iter().map(|claim| claim.claim_id.clone()).collect()
    }

    /// What the turn commits after the failed checkpoint: the claims it still
    /// drives, and the withheld work its failure path hands back.
    fn drive<'c>(_: &'c (), cx: &'c LocalTestCx) -> Pin<Box<dyn Future<Output = ()> + 'c>> {
        Box::pin(async move {
            let admitted = queued_claim("admitted", "batch-a");
            let mut pending_queue_claims = vec![admitted.clone()];
            let mut pending_turn_input_claims = Vec::new();
            let mut pending_checkpoint_turn_input_claim = None;
            let mut withheld_terminal_work =
                crate::runtime::logical_turn::WithheldTerminalWork::default();
            let outcome: RuntimeEffectOutcome = cx
                .op(
                    "turn/1/checkpoint/before_completion",
                    "checkpoint",
                    &"before_completion",
                    async move { failed_checkpoint(admitted, queued_claim("fresh", "batch-b")) },
                )
                .await;
            let (result, claims) = outcome.into_checkpoint().expect("a checkpoint outcome");
            absorb_checkpoint_claims(
                TurnClaimSlots {
                    pending_queue_claims: &mut pending_queue_claims,
                    pending_turn_input_claims: &mut pending_turn_input_claims,
                    pending_checkpoint_turn_input_claim: &mut pending_checkpoint_turn_input_claim,
                    withheld_terminal_work: &mut withheld_terminal_work,
                },
                CheckpointKind::BeforeCompletion,
                claims.queued_work_claims,
                claims.turn_input_claim,
            )
            .expect("fold the recorded claim set");
            let handed_back = result
                .is_err()
                .then(|| withheld_terminal_work.take_if_any())
                .flatten()
                .map(|withheld| claim_ids(&withheld.queued))
                .unwrap_or_default();
            cx.record_commit(&(claim_ids(&pending_queue_claims), handed_back));
        })
    }

    #[test]
    fn a_failed_terminal_checkpoint_hands_back_its_claims_on_every_replay() {
        let engine = LocalEngine::new(|| (), drive);
        let report = DeterminismCheck::new(0x3672_0007)
            .perturbed_replays(6)
            .run(&engine)
            .unwrap_or_else(|failure| panic!("{failure}"));

        assert_eq!(
            report.transcript.commits().collect::<Vec<_>>(),
            vec![r#"[["admitted"],["fresh"]]"#],
            "the admitted claim stays the turn's; the fresh one is withheld and handed back"
        );
    }

    /// The shape this replaced: the step body left its claims in a side
    /// channel on the worker, and the driver read them after the step. A
    /// replay never runs the body, so the claims it hands back differ.
    #[test]
    fn claims_read_from_a_worker_side_channel_diverge_on_replay() {
        #[derive(Default)]
        struct Worker {
            side_channel: std::sync::Mutex<Vec<crate::QueuedWorkClaim>>,
        }
        let engine = LocalEngine::new(Worker::default, |worker: &Worker, cx: &LocalTestCx| {
            Box::pin(async move {
                let _: RuntimeEffectOutcome = cx
                    .op(
                        "turn/1/checkpoint/before_completion",
                        "checkpoint",
                        &"before_completion",
                        async move {
                            let fresh = queued_claim("fresh", "batch-b");
                            worker.side_channel.lock_recover().push(fresh.clone());
                            failed_checkpoint(queued_claim("admitted", "batch-a"), fresh)
                        },
                    )
                    .await;
                let handed_back =
                    claim_ids(&std::mem::take(&mut *worker.side_channel.lock_recover()));
                cx.record_commit(&(vec!["admitted"], handed_back));
            }) as Pin<Box<dyn Future<Output = ()> + '_>>
        });
        let failure = DeterminismCheck::new(0x3672_0007)
            .run(&engine)
            .expect_err("the side channel is not recorded");

        assert_eq!(failure.mode, RunMode::Replay(ReplayMode::Cold));
        assert!(
            matches!(failure.cause, FailureCause::Diverged(_)),
            "{failure}"
        );
    }
}
