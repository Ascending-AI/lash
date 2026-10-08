use super::*;
use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;

/// The rows of one admitted set, keyed by row id.
trait AdmittedRows {
    type Row;

    fn rows(&self) -> &[Self::Row];
    fn row_key(row: &Self::Row) -> &str;
    fn retain_rows(&mut self, overlapping: &std::collections::BTreeSet<String>);
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
    pending_turn_inputs: &'a mut Vec<crate::AdmittedTurnInputs>,
    pending_checkpoint_turn_inputs: &'a mut Option<crate::AdmittedTurnInputs>,
}

/// Folds a checkpoint's recorded admitted set into the turn's rows. A row
/// the turn already executes is a replay of its own work: only rows no set
/// of the turn covers are new work for the pending checkpoint slot.
fn absorb_checkpoint_admissions(
    slots: TurnAdmissionSlots<'_>,
    turn_inputs: Option<crate::AdmittedTurnInputs>,
) -> Result<(), RuntimeError> {
    let TurnAdmissionSlots {
        pending_turn_inputs,
        pending_checkpoint_turn_inputs,
    } = slots;
    if let Some(mut admitted) = turn_inputs {
        drop_held_rows(pending_turn_inputs, &mut admitted);
        if !admitted.inputs.is_empty() {
            merge_pending_checkpoint_turn_inputs(pending_checkpoint_turn_inputs, admitted)?;
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
        event_tx: &TurnObserver,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        // A store that did not answer the admission is this attempt's
        // fault, never the checkpoint's outcome: the turn stops uncommitted
        // and the session's next pass recomputes it from its last phase.
        let admission = self
            .checkpoint_admission(checkpoint)
            .await
            .map_err(|fault| {
                RuntimeEffectControllerError::from(fault).retryable_uncommitted_derivation()
            })?;
        let result = self
            .run_checkpoint(
                messages,
                protocol_iteration,
                checkpoint,
                admission,
                event_tx,
            )
            .await
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
                // mutations to the driver's resident sets.
                turn_inputs: self.pending_checkpoint_turn_inputs.clone(),
                incorporation: self.opener_state.ledger_snapshot(),
            }),
        })
    }

    /// What `checkpoint` admits to this turn (ADR 0101 §5): the steering
    /// input addressed to it whose boundary the checkpoint reaches, in
    /// ingress order. Nothing binds here. The rows bind to the run in the phase commit that
    /// records their delivery (ADR 0132 §4), so a crash before it leaves them
    /// open, and the checkpoint the resumed turn recomputes admits them
    /// again; once bound, no checkpoint reads them open.
    ///
    /// The terminal checkpoint admits nothing: the committed finish is the
    /// turn's answer, and what arrives for the turn is the session's next
    /// run once the turn ends (ADR 0101 §3, §5.1).
    async fn checkpoint_admission(
        &self,
        checkpoint: CheckpointKind,
    ) -> Result<crate::store::CheckpointAdmission, RuntimeError> {
        if checkpoint != CheckpointKind::AfterWork {
            return Ok(crate::store::CheckpointAdmission::default());
        }
        let store = self
            .scoped_effect_controller
            .backend()
            .session_store_factory();
        let mut open_inputs = store
            .list_pending_turn_inputs(&self.session_id)
            .await
            .map_err(crate::runtime::runtime_error_from_store_commit)?
            .into_iter()
            .filter(|read| matches!(read.status, crate::PendingTurnInputReadStatus::Open))
            .map(|read| read.input)
            .collect::<Vec<_>>();
        open_inputs.sort_by_key(|input| input.enqueue_seq);
        let addressed_here = |input: &crate::PendingTurnInput| {
            input.state.kind() == crate::TurnInputStateKind::PendingActive
                && input.state.active_turn_id() == Some(&self.turn_id)
        };
        let mut steering = open_inputs
            .into_iter()
            .filter(|input| addressed_here(input) && input.ingress().admits_checkpoint(checkpoint))
            .collect::<Vec<_>>();
        steering.truncate(self.host.core.control.pacing.checkpoint_inputs.get());
        Ok(crate::store::CheckpointAdmission {
            inputs: crate::store::plan_checkpoint_input_admission(
                &self.session_id,
                &self.turn_id,
                checkpoint,
                steering,
            ),
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
            turn_inputs,
            incorporation,
        } = admitted;
        self.opener_state.absorb_ledger(incorporation);
        // The recorded admitted set is the only way the checkpoint's rows
        // reach this driver: the step body ran on a copy of it. It is folded
        // in before the result is read, so a checkpoint that admitted work
        // and then failed hands that work to the failure path on the live
        // pass and on every replay alike.
        self.absorb_checkpoint_admissions(turn_inputs)
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
        turn_inputs: Option<crate::AdmittedTurnInputs>,
    ) -> Result<(), RuntimeError> {
        absorb_checkpoint_admissions(
            TurnAdmissionSlots {
                pending_turn_inputs: &mut self.pending_turn_inputs,
                pending_checkpoint_turn_inputs: &mut self.pending_checkpoint_turn_inputs,
            },
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

    pub(in crate::runtime) async fn run_checkpoint(
        &mut self,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        checkpoint: CheckpointKind,
        admission: crate::store::CheckpointAdmission,
        event_tx: &TurnObserver,
    ) -> Result<
        (
            crate::CheckpointDelivery,
            Vec<crate::plugin::SessionContributions>,
        ),
        RuntimeError,
    > {
        let mut committed_user_messages = Vec::new();
        let crate::store::CheckpointAdmission {
            inputs: turn_input_admission,
        } = admission;
        debug_assert!(
            self.pending_checkpoint_turn_inputs.is_none(),
            "checkpoint admissions must be resolved before another checkpoint runs"
        );
        // Steering input reaches a work checkpoint only:
        // the terminal checkpoint admits nothing, and the committed finish
        // stays the turn's answer (FIG-5293, FIG-5294).
        if let Some(mut admitted) = turn_input_admission {
            drop_held_rows(&self.pending_turn_inputs, &mut admitted);
            if !admitted.inputs.is_empty() {
                let materialized = admitted
                    .materialize_checkpoint_turn_input(&self.turn_id)
                    .await
                    .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err))?;
                self.pending_checkpoint_turn_inputs = Some(admitted);
                committed_user_messages.extend(materialized.messages);
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
        // Observed as the turn's own activity, as its checkpoint record is:
        // a raw stream event reaches no turn report.
        for event in applied.events {
            self.turn_observations
                .observe(event_tx, crate::engine::ObservedEvent::Session(event));
        }

        Ok((
            crate::CheckpointDelivery {
                committed_user_messages,
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
            .with_code_block_graph_key(code_block_graph_key);
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
