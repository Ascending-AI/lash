use super::*;
use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;

/// The rows of one admitted set, keyed by row id.
trait AdmittedRows {
    type Row;

    fn rows(&self) -> &[Self::Row];
    fn row_key(row: &Self::Row) -> &str;
    fn retain_rows(&mut self, overlapping: &std::collections::BTreeSet<String>);
}

impl AdmittedRows for crate::AdmittedQueuedWork {
    type Row = crate::QueuedWorkBatch;

    fn rows(&self) -> &[Self::Row] {
        &self.batches
    }

    fn row_key(row: &Self::Row) -> &str {
        &row.batch_id
    }

    fn retain_rows(&mut self, overlapping: &std::collections::BTreeSet<String>) {
        self.batches
            .retain(|batch| !overlapping.contains(Self::row_key(batch)));
    }
}

impl AdmittedRows for crate::AdmittedTurnInputs {
    type Row = crate::PendingTurnInput;

    fn rows(&self) -> &[Self::Row] {
        &self.inputs
    }

    fn row_key(row: &Self::Row) -> &str {
        &row.input_id
    }

    fn retain_rows(&mut self, overlapping: &std::collections::BTreeSet<String>) {
        self.inputs
            .retain(|input| !overlapping.contains(Self::row_key(input)));
        self.applications
            .retain(|application| !overlapping.contains(application.input_id.as_str()));
    }
}

/// Drop from `incoming` every row one of `held` already names, and answer
/// those rows' ids.
///
/// A row is admitted to one run once (FIG-3927), so a row two admitted sets
/// both name is the same binding reported twice — a replayed checkpoint
/// outcome re-delivering the rows its turn already executes — never a second
/// authority over it. The set that already holds it keeps it.
fn drop_held_rows<C: AdmittedRows>(
    held: &[C],
    incoming: &mut C,
) -> std::collections::BTreeSet<String> {
    let overlapping = held
        .iter()
        .flat_map(|held| held.rows().iter())
        .map(|row| C::row_key(row))
        .filter(|key| {
            incoming
                .rows()
                .iter()
                .any(|incoming_row| C::row_key(incoming_row) == *key)
        })
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    if !overlapping.is_empty() {
        incoming.retain_rows(&overlapping);
    }
    overlapping
}

/// Whether an incoming queued-work admission names a batch this turn already
/// executes, which marks it as a replay of the turn's own work rather than new
/// work.
fn shares_queued_batches(
    pending: &[crate::AdmittedQueuedWork],
    incoming: &crate::AdmittedQueuedWork,
) -> bool {
    pending.iter().any(|pending| {
        pending.batches.iter().any(|pending_batch| {
            incoming
                .batches
                .iter()
                .any(|incoming_batch| incoming_batch.batch_id == pending_batch.batch_id)
        })
    })
}

/// Whether an incoming turn-input admission names a row this turn already
/// executes.
fn shares_turn_input_rows(
    pending: &[crate::AdmittedTurnInputs],
    incoming: &crate::AdmittedTurnInputs,
) -> bool {
    pending.iter().any(|pending| {
        pending.inputs.iter().any(|pending_input| {
            incoming
                .inputs
                .iter()
                .any(|incoming_input| incoming_input.input_id == pending_input.input_id)
        })
    })
}

fn merge_queued_admission(
    pending: &mut Vec<crate::AdmittedQueuedWork>,
    mut incoming: crate::AdmittedQueuedWork,
) {
    drop_held_rows(pending, &mut incoming);
    if !incoming.batches.is_empty() {
        pending.push(incoming);
    }
}

fn merge_pending_checkpoint_turn_inputs(
    pending: &mut Option<crate::AdmittedTurnInputs>,
    incoming: crate::AdmittedTurnInputs,
) -> Result<(), RuntimeError> {
    match pending.as_ref() {
        None => *pending = Some(incoming),
        Some(existing) if existing.input_ids() == incoming.input_ids() => {}
        Some(existing) => {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                format!(
                    "checkpoint replay returned turn inputs {:?} while {:?} are pending",
                    incoming.input_ids(),
                    existing.input_ids()
                ),
            ));
        }
    }
    Ok(())
}

/// The admitted sets a turn holds, as the checkpoint fold updates them.
struct TurnAdmissionSlots<'a> {
    pending_queued: &'a mut Vec<crate::AdmittedQueuedWork>,
    pending_turn_inputs: &'a mut Vec<crate::AdmittedTurnInputs>,
    pending_checkpoint_turn_inputs: &'a mut Option<crate::AdmittedTurnInputs>,
    withheld_terminal_work: &'a mut crate::runtime::logical_turn::WithheldTerminalWork,
}

/// Folds a checkpoint's recorded admitted set into the turn's rows by the
/// rule the step body applied when it admitted them: at a terminal
/// checkpoint, a row this turn does not already shift is withheld work, not
/// this turn's to settle.
fn absorb_checkpoint_admissions(
    slots: TurnAdmissionSlots<'_>,
    checkpoint: CheckpointKind,
    queued_work: Vec<crate::AdmittedQueuedWork>,
    turn_inputs: Option<crate::AdmittedTurnInputs>,
) -> Result<(), RuntimeError> {
    let TurnAdmissionSlots {
        pending_queued,
        pending_turn_inputs,
        pending_checkpoint_turn_inputs,
        withheld_terminal_work,
    } = slots;
    let withholds_admitted_work = matches!(checkpoint, CheckpointKind::BeforeCompletion);
    for queued in queued_work {
        if withholds_admitted_work && !shares_queued_batches(pending_queued, &queued) {
            merge_queued_admission(&mut withheld_terminal_work.queued, queued);
        } else {
            merge_queued_admission(pending_queued, queued);
        }
    }
    if let Some(mut admitted) = turn_inputs {
        if withholds_admitted_work
            && !shares_turn_input_rows(pending_turn_inputs, &admitted)
            && !pending_checkpoint_turn_inputs
                .as_ref()
                .is_some_and(|pending| {
                    shares_turn_input_rows(std::slice::from_ref(pending), &admitted)
                })
        {
            drop_held_rows(&withheld_terminal_work.turn_inputs, &mut admitted);
            if !admitted.inputs.is_empty() {
                withheld_terminal_work.turn_inputs.push(admitted);
            }
        } else {
            // A replayed checkpoint outcome can re-deliver rows this turn
            // already executes — the withheld rows a follow-on turn was admitted
            // with are carried by the journaled admitted set. Only rows no
            // shift covers are new work for the pending checkpoint slot.
            drop_held_rows(pending_turn_inputs, &mut admitted);
            if !admitted.inputs.is_empty() {
                merge_pending_checkpoint_turn_inputs(pending_checkpoint_turn_inputs, admitted)?;
            }
        }
    }
    Ok(())
}

impl RuntimeTurnDriver<'_> {
    pub(in crate::runtime) async fn execute_checkpoint_locally(
        &mut self,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        checkpoint: CheckpointKind,
        step: &str,
        event_tx: &TurnObserver,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let result = match self.admit_at_checkpoint(checkpoint, step).await {
            Ok(admission) => {
                self.run_checkpoint(
                    messages,
                    protocol_iteration,
                    checkpoint,
                    step,
                    admission,
                    event_tx,
                )
                .await
            }
            // A store that did not answer the admission is this attempt's
            // fault, never the checkpoint's outcome (FIG-4651, as FIG-3683
            // and FIG-3726 rule for the other store-reading steps): the step
            // stays unrecorded and the engine runs it again. A hook's own
            // failure, and an answer the store did give, are still recorded:
            // a superseded shift fence ends the run typed, with no retry.
            // Use the engine's retry policy here too: a superseded commit
            // is a live fault whose recorded fence no retry can repair.
            Err(fault) if crate::runtime::shift::engine_retries(&fault) => {
                return Err(
                    RuntimeEffectControllerError::from(fault).retryable_uncommitted_derivation()
                );
            }
            Err(refusal) => Err(refusal),
        }
        .map_err(RuntimeEffectControllerError::from);
        let (result, session_contributions) = match result {
            Ok((delivery, contributions)) => (Ok(delivery), contributions),
            Err(error) => (Err(error), Vec::new()),
        };
        Ok(RuntimeEffectOutcome::Checkpoint {
            result,
            admitted: Box::new(crate::runtime::effect::CheckpointAdmittedSet {
                session_contributions,
                // A checkpoint outcome is a self-contained snapshot of the
                // rows the turn holds. Replay must never reconstruct it from
                // mutations to the driver's resident sets. Work withheld from
                // a terminal checkpoint (FIG-3157) is part of it: replay
                // routes it back by the same rule that withheld it.
                queued_work: self
                    .pending_queued
                    .iter()
                    .chain(self.withheld_terminal_work.queued.iter())
                    .cloned()
                    .collect(),
                turn_inputs: self
                    .pending_checkpoint_turn_inputs
                    .clone()
                    .or_else(|| self.withheld_terminal_work.turn_inputs.last().cloned()),
                incorporation: self.opener_state.ledger_snapshot(),
            }),
        })
    }

    pub(super) async fn invoke_turn_checkpoint_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        checkpoint: CheckpointKind,
        event_tx: &TurnObserver,
    ) -> Result<crate::CheckpointDelivery, RuntimeEffectControllerError> {
        let invocation = self.turn_effect_invocation(machine, id, RuntimeEffectKind::Checkpoint)?;
        let (result, admitted) = self
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
        let crate::runtime::effect::CheckpointAdmittedSet {
            session_contributions,
            queued_work,
            turn_inputs,
            incorporation,
        } = admitted;
        // A replayed checkpoint is the authority for everything the turn
        // incorporated and enqueued before it. The cells before it re-run on
        // replay (ADR 0103) and re-incorporate the same settlements, which
        // refills the checkpoint message buffer the live checkpoint drained.
        // The recorded delivery already carries those messages, so the refill
        // is discarded here; after a live checkpoint the buffer is already
        // empty and this drains nothing.
        self.checkpoint_messages.drain();
        self.opener_state.absorb_ledger(incorporation);
        // The recorded admitted set is the only way the checkpoint's rows
        // reach this driver: the step body ran on a copy of it. It is folded
        // in before the result is read, so a checkpoint that admitted work
        // and then failed hands that work to the failure path on the live
        // pass and on every replay alike.
        self.absorb_checkpoint_admissions(checkpoint, queued_work, turn_inputs)
            .map_err(RuntimeEffectControllerError::from)?;
        // A failed checkpoint is part of the checkpoint's own recorded
        // outcome: the journal holds it, and every redrive replays it. It is
        // therefore an outcome whatever its code (FIG-3528, FIG-3575), never
        // an abort a redrive would reproduce forever.
        let delivery = result.map_err(RuntimeEffectControllerError::into_journaled)?;
        self.turn_pipeline
            .graph_appends()
            .apply_session_contributions(
                &self.session_id,
                self.session.plugins(),
                &session_contributions,
            )
            .map_err(|err| {
                RuntimeEffectControllerError::from(
                    err.into_turn_failure(RuntimeErrorCode::PluginCheckpoint),
                )
            })?;
        Ok(delivery)
    }

    fn absorb_checkpoint_admissions(
        &mut self,
        checkpoint: CheckpointKind,
        queued_work: Vec<crate::AdmittedQueuedWork>,
        turn_inputs: Option<crate::AdmittedTurnInputs>,
    ) -> Result<(), RuntimeError> {
        absorb_checkpoint_admissions(
            TurnAdmissionSlots {
                pending_queued: &mut self.pending_queued,
                pending_turn_inputs: &mut self.pending_turn_inputs,
                pending_checkpoint_turn_inputs: &mut self.pending_checkpoint_turn_inputs,
                withheld_terminal_work: &mut self.withheld_terminal_work,
            },
            checkpoint,
            queued_work,
            turn_inputs,
        )
    }

    /// Phase 2 of [`RuntimeTurnDriver::invoke_turn_llm_effect`].
    pub(super) async fn invoke_assistant_response_hooks_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        response: LlmResponse,
        plan: crate::runtime::AssistantResponsePlan,
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
                        plan,
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
                self.turn_observations
                    .observe(event_tx, crate::engine::ObservedEvent::Session(event));
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
        code: String,
        event_tx: &TurnObserver,
    ) -> Result<Result<crate::ExecResponse, crate::ExecCodeFailure>, RuntimeEffectControllerError>
    {
        self.execute_typed_turn_effect(
            machine,
            event_tx,
            RuntimeEffectEnvelope::new(invocation, RuntimeEffectCommand::ExecCode { code }),
            RuntimeEffectOutcome::into_exec_code,
        )
        .await
    }

    /// The store's admission at a checkpoint. Only a turn that runs under an
    /// admitted run admits at its checkpoints: the rows bind to that run,
    /// keyed by this step, under the run's shift fence (FIG-3927).
    async fn admit_at_checkpoint(
        &self,
        checkpoint: CheckpointKind,
        step: &str,
    ) -> Result<crate::store::CheckpointAdmission, RuntimeError> {
        let (Some(store), Some(fence), Some(run)) = (
            self.session.history_store(),
            self.shift_fence.as_ref(),
            self.shift_run.as_ref(),
        ) else {
            return Ok(crate::store::CheckpointAdmission::default());
        };
        let policy = self
            .host
            .core
            .durability
            .queued_work_batching
            .admission_policy(self.policy.llm_profile_config().context_window_tokens());
        store
            .admit_at_checkpoint(&crate::store::CheckpointAdmissionRequest {
                fence: fence.clone(),
                run: run.clone(),
                turn_id: self.turn_id.clone(),
                checkpoint,
                step: step.to_string(),
                max_inputs: 64,
                policy,
            })
            .await
            .map_err(crate::runtime::runtime_error_from_store_commit)
    }

    pub(in crate::runtime) async fn run_checkpoint(
        &mut self,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        checkpoint: CheckpointKind,
        step: &str,
        admission: crate::store::CheckpointAdmission,
        event_tx: &TurnObserver,
    ) -> Result<
        (
            crate::CheckpointDelivery,
            Vec<crate::plugin::SessionContributions>,
        ),
        RuntimeError,
    > {
        let mut committed = self.checkpoint_messages.drain();
        let mut transient_messages = Vec::new();
        let mut committed_user_messages = Vec::new();
        let mut turn_causes = Vec::new();
        if let Some(run) = self.shift_run.as_ref()
            && !admission.is_empty()
        {
            let causes = admission
                .queued
                .as_ref()
                .map(|queued| queued.materialize_queued_checkpoint_work().turn_causes)
                .unwrap_or_default();
            self.emit_trace(protocol_iteration, || lash_trace::TraceEvent::Custom {
                name: "ingress.admitted".to_string(),
                payload: ingress_admitted_trace_payload(
                    run,
                    step,
                    crate::AdmissionBoundary::ActiveTurnCheckpoint,
                    admission.inputs.as_ref(),
                    admission.queued.as_ref(),
                    &causes,
                ),
            });
        }
        let crate::store::CheckpointAdmission {
            inputs: turn_input_admission,
            queued: queued_admission,
        } = admission;
        debug_assert!(
            self.pending_checkpoint_turn_inputs.is_none(),
            "checkpoint admissions must be resolved before another checkpoint runs"
        );
        // FIG-3157: a terminal finish ends the turn, so work admitted at this
        // boundary never extends it. It is withheld from the delivery and
        // starts a follow-on turn inside the same logical run instead. The
        // boundary that admitted it is still the boundary it reports.
        let withholds_admitted_work = matches!(checkpoint, CheckpointKind::BeforeCompletion);
        if let Some(mut admitted) = turn_input_admission {
            let already_delivered = drop_held_rows(&self.pending_turn_inputs, &mut admitted);
            // Rows this turn already executes are a replay of its own work, not
            // new input: they settle with the turn rather than starting a
            // turn of their own.
            if withholds_admitted_work
                && already_delivered.is_empty()
                && !admitted.inputs.is_empty()
            {
                drop_held_rows(&self.withheld_terminal_work.turn_inputs, &mut admitted);
                // The row was accepted at this boundary; only the turn that
                // renders it moves. Applications are recorded by that turn.
                let accepted_turn_inputs = admitted.accepted_turn_inputs();
                self.withheld_terminal_work.turn_inputs.push(admitted);
                if !accepted_turn_inputs.is_empty() {
                    self.turn_observations.observe(
                        event_tx,
                        crate::engine::ObservedEvent::Session(
                            SessionStreamEvent::InjectedTurnInputAccepted {
                                inputs: accepted_turn_inputs,
                                checkpoint,
                            },
                        ),
                    );
                }
            } else if !admitted.inputs.is_empty() {
                let materialized = admitted
                    .materialize_checkpoint_turn_input(
                        &self.turn_id,
                        self.host.core.durability.attachment_store.as_ref(),
                        self.host.core.attachment_source_policy.as_ref(),
                    )
                    .await
                    .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err))?;
                self.pending_checkpoint_turn_inputs = Some(admitted);
                committed_user_messages.extend(materialized.messages);
                turn_causes.extend(materialized.turn_causes);
            }
        }
        if let Some(queued) = queued_admission {
            let materialized = queued.materialize_queued_checkpoint_work();
            send_queued_work_started_event(
                event_tx,
                &mut self.turn_observations,
                crate::AdmissionBoundary::ActiveTurnCheckpoint,
                &queued,
                materialized.turn_causes.clone(),
            );
            if withholds_admitted_work && !shares_queued_batches(&self.pending_queued, &queued) {
                merge_queued_admission(&mut self.withheld_terminal_work.queued, queued);
            } else {
                turn_causes.extend(materialized.turn_causes);
                merge_queued_admission(&mut self.pending_queued, queued);
            }
        }
        let plugins = Arc::clone(self.session.plugins());
        let applied = plugins
            .apply_checkpoint(CheckpointHookContext {
                session_id: self.session_id.clone(),
                plugin_config: plugins.admitted_plugin_config(),
                checkpoint,
                state: self.checkpoint_state_view(messages, protocol_iteration),
                sessions: self.session_services.read_service(),
            })
            .await
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginCheckpoint))?;
        committed.extend(applied.messages);
        emit_session_events(event_tx, applied.events);
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
            self.turn_observations.observe(
                event_tx,
                crate::engine::ObservedEvent::Session(
                    SessionStreamEvent::InjectedMessagesCommitted {
                        messages: committed.clone(),
                        checkpoint,
                    },
                ),
            );
        }

        Ok((
            crate::CheckpointDelivery {
                committed_user_messages,
                messages: committed,
                transient_messages,
                turn_causes,
            },
            applied.session,
        ))
    }

    pub(in crate::runtime) async fn run_exec_code(
        &self,
        code: &str,
        chronological_projection: Arc<crate::facade_support::ChronologicalProjection>,
        protocol_iteration: usize,
        invocation: crate::RuntimeInvocation,
        event_tx: &TurnObserver,
    ) -> Result<
        Result<crate::ExecResponse, crate::ExecCodeFailure>,
        crate::RuntimeEffectControllerError,
    > {
        let code_executor = self.session.plugins().code_executor();
        let code_block_graph_key = foreground_exec_graph_key(&invocation);
        let context = self
            .execution_context(event_tx, chronological_projection)
            .map_err(crate::RuntimeEffectControllerError::from)?
            .with_tracing(self.execution_tracing(protocol_iteration))
            .with_code_block_graph_key(code_block_graph_key)
            .with_turn_hand_over(false);
        let context = context.with_parent_invocation(invocation);
        let result = match code_executor {
            Some(code_executor) => code_executor
                .execute_code(
                    context.clone(),
                    crate::ExecRequest {
                        code: code.to_string(),
                    },
                )
                .await
                .map_err(|e| e.to_exec_code_failure()),
            None => Err(crate::SessionError::CodeExecutionUnavailable.to_exec_code_failure()),
        };
        let nested_effect_error = context.take_nested_effect_error();
        drop(context);
        match nested_effect_error {
            Some(error) => Err(error),
            None => Ok(result),
        }
    }
}

pub(in crate::runtime) async fn normalize_plugin_message_attachments(
    messages: &mut [crate::PluginMessage],
    attachment_store: &crate::RuntimeAttachmentStore,
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
    attachment_store: &crate::RuntimeAttachmentStore,
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
