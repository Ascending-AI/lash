use super::*;

fn compare_queue_claim_authority(
    left: &crate::QueuedWorkClaim,
    right: &crate::QueuedWorkClaim,
) -> std::cmp::Ordering {
    (left.session_lease_generation, left.fencing_token)
        .cmp(&(right.session_lease_generation, right.fencing_token))
}

fn merge_pending_queue_claim_authority(
    pending_claims: &mut Vec<crate::QueuedWorkClaim>,
    mut incoming: crate::QueuedWorkClaim,
) -> Result<(), RuntimeError> {
    for pending in pending_claims.iter_mut() {
        let overlapping = pending
            .batches
            .iter()
            .filter(|pending_batch| {
                incoming
                    .batches
                    .iter()
                    .any(|batch| batch.batch_id == pending_batch.batch_id)
            })
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>();
        if overlapping.is_empty() {
            continue;
        }

        match compare_queue_claim_authority(pending, &incoming) {
            std::cmp::Ordering::Less => pending
                .batches
                .retain(|batch| !overlapping.contains(&batch.batch_id)),
            std::cmp::Ordering::Greater => incoming
                .batches
                .retain(|batch| !overlapping.contains(&batch.batch_id)),
            std::cmp::Ordering::Equal
                if pending.claim_id == incoming.claim_id
                    && pending.lease_token == incoming.lease_token =>
            {
                incoming
                    .batches
                    .retain(|batch| !overlapping.contains(&batch.batch_id));
            }
            std::cmp::Ordering::Equal => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!(
                        "queued-work rows {overlapping:?} have conflicting claim authorities `{}` and `{}` at session generation {} and fencing token {}",
                        pending.claim_id,
                        incoming.claim_id,
                        incoming.session_lease_generation,
                        incoming.fencing_token,
                    ),
                ));
            }
        }
    }
    pending_claims.retain(|claim| !claim.batches.is_empty());
    if !incoming.batches.is_empty() {
        pending_claims.push(incoming);
    }
    Ok(())
}

/// Reconcile a checkpoint's fresh claim against the claims this turn already
/// holds.
///
/// Only claimed drives take part: a checkpoint claim exists only under a
/// session-execution fence, and a turn that holds that fence took its own rows
/// under a claim too. An unclaimed drive therefore never overlaps an incoming
/// checkpoint claim in this process — and if two processes ever did reach that
/// state, the head CAS refuses one of them, because a claimed row no longer
/// satisfies the unclaimed settlement predicate (ADR 0069 §5).
fn merge_pending_turn_input_claim_authority(
    pending_drives: &mut Vec<crate::runtime::turn_input_ingress::TurnInputDrive>,
    incoming: &mut crate::TurnInputClaim,
) -> Result<std::collections::HashSet<String>, RuntimeError> {
    let mut already_delivered = std::collections::HashSet::new();
    let mut pending_claims = pending_drives
        .iter_mut()
        .filter_map(|drive| match drive {
            crate::runtime::turn_input_ingress::TurnInputDrive::Claimed(claim) => Some(claim),
            crate::runtime::turn_input_ingress::TurnInputDrive::Unclaimed(_) => None,
        })
        .collect::<Vec<_>>();
    for pending in pending_claims.iter_mut() {
        let overlapping = pending
            .inputs
            .iter()
            .filter(|pending_input| {
                incoming
                    .inputs
                    .iter()
                    .any(|input| input.input_id == pending_input.input_id)
            })
            .map(|input| input.input_id.clone())
            .collect::<std::collections::HashSet<_>>();
        if overlapping.is_empty() {
            continue;
        }

        match (pending.session_lease_generation, pending.fencing_token)
            .cmp(&(incoming.session_lease_generation, incoming.fencing_token))
        {
            std::cmp::Ordering::Less => {
                already_delivered.extend(overlapping.iter().cloned());
                for application in pending
                    .applications
                    .iter()
                    .filter(|application| overlapping.contains(&application.input_id))
                {
                    if !incoming
                        .applications
                        .iter()
                        .any(|existing| existing.input_id == application.input_id)
                    {
                        incoming.applications.push(application.clone());
                    }
                }
                pending
                    .inputs
                    .retain(|input| !overlapping.contains(&input.input_id));
                pending
                    .applications
                    .retain(|application| !overlapping.contains(&application.input_id));
            }
            std::cmp::Ordering::Greater => {
                incoming
                    .inputs
                    .retain(|input| !overlapping.contains(&input.input_id));
                incoming
                    .applications
                    .retain(|application| !overlapping.contains(&application.input_id));
            }
            std::cmp::Ordering::Equal
                if pending.claim_id == incoming.claim_id
                    && pending.lease_token == incoming.lease_token =>
            {
                incoming
                    .inputs
                    .retain(|input| !overlapping.contains(&input.input_id));
                incoming
                    .applications
                    .retain(|application| !overlapping.contains(&application.input_id));
            }
            std::cmp::Ordering::Equal => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!(
                        "turn-input rows {overlapping:?} have conflicting claim authorities `{}` and `{}` at session generation {} and fencing token {}",
                        pending.claim_id,
                        incoming.claim_id,
                        incoming.session_lease_generation,
                        incoming.fencing_token,
                    ),
                ));
            }
        }
    }
    drop(pending_claims);
    pending_drives.retain(|drive| !drive.inputs().is_empty());
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
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
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
                // driver's resident claim set.
                queued_work_claims: self.pending_queue_claims.clone(),
                turn_input_claim: self.pending_checkpoint_turn_input_claim.clone(),
            }),
        }
    }

    pub(super) async fn invoke_turn_checkpoint_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        checkpoint: CheckpointKind,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<crate::CheckpointDelivery, RuntimeError> {
        let invocation = self
            .turn_effect_invocation(machine, id, RuntimeEffectKind::Checkpoint)
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let (result, queued_work_claims, turn_input_claim) = self
            .execute_typed_turn_effect(
                machine,
                event_tx,
                cancel,
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::Checkpoint { checkpoint },
                ),
                RuntimeEffectOutcome::into_checkpoint,
            )
            .await
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let delivery = result.map_err(RuntimeEffectControllerError::into_runtime_error)?;
        for claim in queued_work_claims {
            self.merge_pending_queue_claim_authority(claim)?;
        }
        if let Some(claim) = turn_input_claim {
            merge_pending_checkpoint_turn_input_claim(
                &mut self.pending_checkpoint_turn_input_claim,
                claim,
            )?;
        }
        Ok(delivery)
    }

    /// Phase 2 of [`RuntimeTurnDriver::invoke_turn_llm_effect`].
    pub(super) async fn invoke_assistant_response_hooks_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        response: LlmResponse,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<LlmResponse, RuntimeEffectControllerError> {
        // Rebuilt rather than threaded through: phase 1's invocation is a pure
        // function of the same turn identity, so this is the identical parent
        // and the causal edge survives a redrive that only runs phase 2.
        let phase_one = self.turn_effect_invocation(machine, id, RuntimeEffectKind::LlmCall)?;
        let invocation = crate::runtime::causal::turn_phase_effect_invocation(
            &phase_one,
            id,
            RuntimeEffectKind::AssistantResponseHooks,
        );
        let (response, events) = self
            .execute_typed_turn_effect(
                machine,
                event_tx,
                cancel,
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::AssistantResponseHooks {
                        response: Box::new(response),
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
                send_session_event(event_tx, event).await;
            }
        }
        Ok(response)
    }

    pub(super) async fn invoke_turn_execution_environment_sync_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        update_machine_config: bool,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<
        Result<Option<crate::sansio::ExecutionEnvironmentSync>, String>,
        RuntimeEffectControllerError,
    > {
        let invocation =
            self.turn_effect_invocation(machine, id, RuntimeEffectKind::SyncExecutionEnvironment)?;
        self.execute_typed_turn_effect(
            machine,
            event_tx,
            cancel,
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::SyncExecutionEnvironment {
                    update_machine_config,
                },
            ),
            RuntimeEffectOutcome::into_sync_execution_environment,
        )
        .await
    }

    pub(super) async fn invoke_turn_exec_effect(
        &mut self,
        machine: &mut TurnMachine,
        invocation: crate::RuntimeInvocation,
        language: String,
        code: String,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<Result<crate::ExecResponse, String>, RuntimeEffectControllerError> {
        self.execute_typed_turn_effect(
            machine,
            event_tx,
            cancel,
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
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
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
        self.pending_checkpoint_turn_input_claim = turn_input_claim;
        if let Some(claim) = self.pending_checkpoint_turn_input_claim.as_mut() {
            let already_delivered = merge_pending_turn_input_claim_authority(
                &mut self.pending_turn_input_claims,
                claim,
            )?;
            let mut delivery_claim = claim.clone();
            delivery_claim
                .inputs
                .retain(|input| !already_delivered.contains(&input.input_id));
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
        if let Some(claim) = queue_claim {
            let materialized = claim
                .materialize_queued_checkpoint_work_with_attachments(
                    self.host.core.durability.attachment_store.as_ref(),
                )
                .await
                .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err))?;
            send_queued_work_started_event(
                event_tx,
                crate::QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                &claim,
                materialized.turn_causes.clone(),
            )
            .await;
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
            committed.extend(materialized.messages);
            transient_messages.extend(materialized.transient_messages);
            turn_causes.extend(materialized.turn_causes);
            self.merge_pending_queue_claim_authority(claim)?;
        }
        let plugins = Arc::clone(self.session.plugins());
        let applied = plugins
            .apply_checkpoint(CheckpointHookContext {
                session_id: self.session_id.clone(),
                checkpoint,
                state: self.checkpoint_state_view(messages, protocol_iteration),
                sessions: self.session_services.state_service(),
                session_lifecycle: self.session_services.lifecycle_service(),
                session_graph: self.session_services.graph_service(),
            })
            .await
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginCheckpoint, err.to_string())
            })?;
        committed.extend(applied.messages);
        emit_session_events(event_tx, applied.events).await;
        if let Some(abort) = applied.abort {
            return Err(RuntimeError::new(
                RuntimeErrorCode::from_wire_code(&abort.code),
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
            send_session_event(
                event_tx,
                SessionStreamEvent::InjectedMessagesCommitted {
                    messages: committed.clone(),
                    checkpoint,
                },
            )
            .await;
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
        invocation: crate::RuntimeInvocation,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancellation: &CancellationToken,
    ) -> Result<Result<crate::ExecResponse, String>, crate::RuntimeEffectControllerError> {
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
                        send_session_event(&relay_tx, event).await;
                    }
                    maybe_turn_event = turn_event_rx.recv(), if !turn_closed => {
                        let Some(event) = maybe_turn_event else {
                            turn_closed = true;
                            continue;
                        };
                        let _ = relay_tx.send(RuntimeStreamEvent::Turn(event)).await;
                    }
                }
            }
        });
        let code_executor = self.session.plugins().code_executor();
        let read_view = self.checkpoint_state_view(messages, protocol_iteration);
        let chronological_projection = read_view.shared_chronological_projection();
        let code_block_graph_key = foreground_exec_graph_key(&invocation);
        let context = self
            .execution_context(session_event_tx.clone(), chronological_projection)
            .map_err(crate::RuntimeEffectControllerError::from)?
            .with_turn_event_sender(turn_event_tx.clone())
            .with_tracing(self.execution_tracing(protocol_iteration))
            .with_code_block_graph_key(code_block_graph_key);
        let context = context.with_parent_invocation(invocation);
        let context = context.with_cancellation_token(cancellation.clone());
        let result = match code_executor {
            Some(code_executor) => code_executor
                .execute_code(
                    context.clone(),
                    crate::ExecRequest {
                        language,
                        code: code.to_string(),
                        accept_finish: true,
                    },
                )
                .await
                .map_err(|e| e.to_string()),
            None => Err(crate::SessionError::CodeExecutionUnavailable.to_string()),
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
        for source in &mut message.attachments {
            normalize_plugin_attachment_source(source, attachment_store, policy).await?;
        }
        for part in &mut message.parts {
            if let Some(attachment) = part.attachment.as_mut() {
                normalize_plugin_attachment_source(
                    &mut attachment.source,
                    attachment_store,
                    policy,
                )
                .await?;
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
            batch_id: batch_id.to_string(),
            session_id: "fig905".to_string(),
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
            session_id: "fig905".to_string(),
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
            input_id: input_id.to_string(),
            session_id: "fig905".to_string(),
            enqueue_seq: 1,
            source_key: None,
            ingress: crate::TurnInputIngress::active_turn(
                "fig905-turn",
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            state: crate::TurnInputState::Accepted,
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
            session_id: "fig905".to_string(),
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
        let mut pending = vec![super::super::turn_input_ingress::TurnInputDrive::Claimed(
            turn_input_claim("first", 2, 3, &["input-a"]),
        )];
        let mut incoming = turn_input_claim("conflicting", 2, 3, &["input-a"]);
        let error = merge_pending_turn_input_claim_authority(&mut pending, &mut incoming)
            .expect_err("equal authority must not silently choose a turn-input claim");

        assert_eq!(error.code, RuntimeErrorCode::StoreCommitFailed);
        assert!(error.message.contains("conflicting claim authorities"));
        assert_eq!(
            pending[0]
                .as_claim()
                .expect("pending drive stays claimed")
                .claim_id,
            "first"
        );
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
