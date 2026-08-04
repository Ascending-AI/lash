use super::*;

impl RuntimeTurnDriver<'_> {
    fn record_pending_queue_claim(&mut self, claim: crate::QueuedWorkClaim) {
        // A later checkpoint can reclaim a replay-restored predecessor claim
        // under the successor session-lease generation. Keep only the newer
        // authority for any overlapping durable batch.
        self.pending_queue_claims.retain(|pending| {
            !pending.batches.iter().any(|pending_batch| {
                claim
                    .batches
                    .iter()
                    .any(|batch| batch.batch_id == pending_batch.batch_id)
            })
        });
        self.pending_queue_claims.push(claim);
    }

    pub(in crate::runtime) async fn execute_checkpoint_locally(
        &mut self,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        checkpoint: CheckpointKind,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    ) -> RuntimeEffectOutcome {
        let prior_queue_claim_count = self.pending_queue_claims.len();
        let result = self
            .run_checkpoint(messages, protocol_iteration, checkpoint, event_tx)
            .await
            .map_err(RuntimeEffectControllerError::from);
        RuntimeEffectOutcome::Checkpoint {
            result,
            claims: Box::new(crate::runtime::effect::CheckpointClaimSet {
                queued_work_claims: self.pending_queue_claims[prior_queue_claim_count..].to_vec(),
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
            self.record_pending_queue_claim(claim);
        }
        if let Some(claim) = turn_input_claim {
            match self.pending_checkpoint_turn_input_claim.as_ref() {
                None => self.pending_checkpoint_turn_input_claim = Some(claim),
                Some(pending) if pending.claim_id == claim.claim_id => {}
                Some(pending) => {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::StoreCommitFailed,
                        format!(
                            "checkpoint replay returned turn-input claim `{}` while `{}` is pending",
                            claim.claim_id, pending.claim_id
                        ),
                    ));
                }
            }
        }
        Ok(delivery)
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
        let mut committed = self.checkpoint_messages.drain().map_err(|err| {
            RuntimeError::new(
                RuntimeErrorCode::Other("checkpoint_messages".to_string()),
                err,
            )
        })?;
        let mut transient_messages = Vec::new();
        let mut committed_user_messages = Vec::new();
        let mut turn_causes = Vec::new();
        let (turn_input_claim, queue_claim) = if let Some(store) = self.session.history_store() {
            if let Some(session_execution_lease) = self.session_execution_lease.as_ref() {
                match store
                    .claim_checkpoint_work(
                        &self.session_id,
                        session_execution_lease,
                        &self.runtime_lease_owner,
                        &self.turn_id,
                        checkpoint,
                        64,
                        self.turn_context.checkpoint_queued_work_limit(64),
                    )
                    .await
                {
                    Ok(claims) => claims,
                    Err(crate::StoreError::SessionExecutionLeaseExpired { .. }) => {
                        tracing::debug!(
                            session_id = %self.session_id,
                            turn_id = %self.turn_id,
                            event = "session_execution_lease.checkpoint_advisory",
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
        if let Some(claim) = self.pending_checkpoint_turn_input_claim.as_ref() {
            let materialized = claim
                .materialize_for_checkpoint(
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
                .materialize_for_checkpoint_with_attachments(
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
            self.record_pending_queue_claim(claim);
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
            return Err(RuntimeError::new(abort.code, abort.message));
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
        &mut self,
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
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<SandboxMessage>();
        self.session.set_message_sender(msg_tx);
        let relay_tx = event_tx.clone();
        let relay_handle = crate::task::spawn(async move {
            let mut sandbox_closed = false;
            let mut session_closed = false;
            let mut turn_closed = false;
            while !(sandbox_closed && session_closed && turn_closed) {
                tokio::select! {
                    biased;
                    maybe_sandbox = msg_rx.recv(), if !sandbox_closed => {
                        let Some(sandbox_msg) = maybe_sandbox else {
                            sandbox_closed = true;
                            continue;
                        };
                        if sandbox_msg.kind != "code" && !relay_tx.is_closed() {
                            let _ = relay_tx
                                .send(RuntimeStreamEvent::Session(SessionStreamEvent::Message {
                                    text: sandbox_msg.text,
                                    kind: sandbox_msg.kind,
                                }))
                                .await;
                        }
                    }
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
        self.session.clear_message_sender();
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
