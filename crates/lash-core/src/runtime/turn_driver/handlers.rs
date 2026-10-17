use super::*;
use crate::runtime::durable::session::CellExit;
use lash_core_llm::stream_assembly::ReasoningPublicationState;

impl RuntimeTurnDriver<'_> {
    fn handle_machine_response(
        &self,
        machine: &mut TurnMachine,
        response: Response,
    ) -> Result<(), RuntimeError> {
        machine
            .try_handle_response(response)
            .map_err(|overflow| self.usage_overflow(overflow))
    }

    fn usage_overflow(
        &self,
        overflow: lash_protocol_types::llm::types::TokenUsageOverflow,
    ) -> RuntimeError {
        crate::runtime::runtime_error_from_store_commit(
            crate::StoreError::TokenUsageAccountingOverflow {
                usage_source: "turn".to_string(),
                model: self
                    .policy
                    .llm_profile_config()
                    .model
                    .wire_model()
                    .to_string(),
                counter: overflow.counter(),
            },
        )
    }

    /// Settle the turn's model calls whose tails drained (FIG-6433): each
    /// call's record is recorded, and its reported usage replaces the
    /// unknown its response was admitted with; a call whose usage never
    /// arrived stays unknown. With `wait`, every drain is waited for, each
    /// bounded by its call's deadline: the turn never closes or parks while
    /// one of its calls is still draining.
    pub(super) async fn settle_call_drains(
        &mut self,
        machine: &mut TurnMachine,
        _event_tx: &TurnObserver,
        wait: bool,
    ) -> Result<(), RuntimeError> {
        for (drained, newest) in self.call_drains.settle(wait).await {
            let usage = drained
                .record
                .usage()
                .map_err(|overflow| self.usage_overflow(overflow))?;
            self.queue_model_call(
                drained.call,
                lash_core_store::model_calls::ModelCallEnd::Answered,
                drained.record,
            );
            {
                machine
                    .settle_unknown_call_usage(&usage, newest)
                    .map_err(|overflow| self.usage_overflow(overflow))?;
            }
        }
        Ok(())
    }

    /// Count `record`'s call unknown in the turn's usage when it ended
    /// unsuccessfully after a response was observed and its provider usage
    /// never arrived: no response admitted it, and it is never a zero.
    fn record_unknown_failed_call(
        &self,
        machine: &mut TurnMachine,
        result: &Result<LlmResponse, crate::LlmCallError>,
        record: Option<&crate::LlmCallRecord>,
    ) -> Result<(), RuntimeError> {
        if result.is_err()
            && let Some(record) = record
        {
            let usage = record
                .usage()
                .map_err(|overflow| self.usage_overflow(overflow))?;
            machine
                .record_unanswered_call_usage(&usage)
                .map_err(|overflow| self.usage_overflow(overflow))?;
        }
        Ok(())
    }

    /// Take the protocol's before-call decision on `request`, the call the
    /// machine waits on as `id`, before the call is admitted. `false` when
    /// the decision ended the turn instead (a frame switch, or a refusal
    /// recorded as a failed turn): nothing is admitted.
    pub(super) async fn before_llm_call(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        request: &LlmRequest,
        event_tx: &TurnObserver,
    ) -> Result<bool, RuntimeError> {
        let invocation = self
            .turn_effect_invocation(machine, id, RuntimeEffectKind::BeforeLlmCall)
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let decision = self
            .execute_typed_turn_effect(
                machine,
                event_tx,
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::BeforeLlmCall {
                        request: Box::new(request.clone()),
                    },
                ),
                RuntimeEffectOutcome::into_before_llm_call,
            )
            .await
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        match decision {
            Ok(Some(crate::ProtocolLlmCallAction::SwitchAgentFrame {
                frame_key,
                task,
                next_run,
            })) => {
                let next_run = next_run.next_run().map_err(|error| {
                    RuntimeError::new(
                        RuntimeErrorCode::RecordEncodingFailed,
                        format!("the frame switch's run spec does not encode: {error}"),
                    )
                })?;
                machine.finish_with_outcome(crate::TurnOutcome::AgentFrameSwitch {
                    frame_key,
                    task,
                    initial_nodes: Vec::new(),
                    next_run,
                });
                Ok(false)
            }
            Ok(None) => Ok(true),
            // A protocol refusal before the model call is an outcome over the
            // turn's journaled inputs, recorded as a failed turn on every host
            // (FIG-3575). Only a live fault the hook ran into, or a session
            // retirement it met (FIG-3630), aborts.
            Err(err) => {
                let failure = err.into_turn_failure(RuntimeErrorCode::ProtocolBeforeLlmCall);
                if failure.turn_failure_cause().aborts_invocation()
                    || failure.is_session_retirement()
                {
                    return Err(failure);
                }
                machine.fail_turn(make_error_event(
                    crate::TurnFailureKind::ProtocolBeforeLlmCall,
                    Some(crate::TurnFailureCode::BeforeLlmCallFailed.into()),
                    failure.message.clone(),
                    Some(failure.message),
                    lash_protocol_types::session_model::RuntimeOutputCuts::standard(),
                ));
                Ok(false)
            }
        }
    }

    /// Send attempt of the admitted call `id`: `admitted` holds its request
    /// template, which every attempt and resend fills and sends, and the
    /// response context recorded with it. The provider's outcome is returned
    /// as the call's receipt, underived, whatever it is: the phase runner
    /// commits it under `model.answer` before anything derives from it
    /// ([`Self::derive_llm_answer`]). `None` when the attempt produced no
    /// provider outcome: its failure answered the machine, or aborts.
    pub(super) async fn handle_llm_call_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        request: Arc<LlmRequest>,
        admitted: &lash_protocol_types::llm::types::AdmittedSend,
        attempt: u32,
        event_tx: &TurnObserver,
    ) -> Result<Option<crate::runtime::durable::session::ModelAnswer>, RuntimeError> {
        self.trace_before_llm_call(machine, &request);
        self.admitted_body = Some(admitted.clone());
        self.model_attempt = attempt;
        let crate::runtime::RuntimeLlmCallOutcome {
            result,
            text_streamed,
            call_record,
            stream,
        } = match self
            .invoke_turn_llm_effect(machine, id, request, event_tx)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(None);
            }
        };
        let call_id = call_record
            .as_ref()
            .map(|record| record.call_id.clone())
            .unwrap_or_else(|| crate::provider::call_id_for_scope(&admitted.response.scope));
        if let Some(record) = call_record {
            let reason = result.as_ref().err().and_then(|error| {
                [
                    (
                        crate::TurnFailureCode::ModelTotalExceeded,
                        crate::provider::AttemptEndReason::ModelDeadline,
                    ),
                    (
                        crate::TurnFailureCode::ProviderPanicked,
                        crate::provider::AttemptEndReason::ProviderPanicked,
                    ),
                    (
                        crate::TurnFailureCode::PluginAssistantStream,
                        crate::provider::AttemptEndReason::StreamConsumerFailed,
                    ),
                    (
                        crate::TurnFailureCode::Cancelled,
                        crate::provider::AttemptEndReason::ImmediateCancel,
                    ),
                ]
                .into_iter()
                .find_map(|(code, reason)| {
                    (error.code == Some(crate::FailureCode::lash(code))).then_some(reason)
                })
            });
            let end = reason.map_or(
                lash_core_store::model_calls::ModelCallEnd::Answered,
                |reason| lash_core_store::model_calls::ModelCallEnd::Unanswered {
                    reason: lash_core_store::model_calls::ModelCallEndReason::Attempt(reason),
                },
            );
            self.queue_model_call(self.model_call_ordinal, end, record);
        }
        Ok(Some(crate::runtime::durable::session::ModelAnswer {
            attempt,
            result,
            text_streamed,
            call_id,
            stream,
        }))
    }

    /// Derive the response of model call `id` from `answer`, its committed
    /// receipt, and answer the machine with it: the call's record is
    /// recorded, its stream is finished for its recorded reason, a
    /// completion is derived through its recorded plan (an empty plan serves
    /// it unchanged) and an unsuccessful outcome answers as it is. Every
    /// derivation, on the worker that sent the call or on one that restored
    /// it, takes this one path and invokes exactly the callbacks the receipt
    /// owes. A hook's live fault aborts the attempt, and the next derives
    /// from the same receipt.
    pub(super) async fn derive_llm_answer(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        answer: crate::runtime::durable::session::ModelAnswer,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let crate::runtime::durable::session::ModelAnswer {
            attempt,
            result,
            text_streamed,
            call_id,
            stream:
                crate::runtime::LlmStreamRecord {
                    attempt: observed_attempt,
                    reasoning_published,
                    capsule,
                    response_plan,
                },
        } = answer;
        self.model_attempt = attempt;
        let call_record = self.recorded_model_call(&call_id).await?;
        if call_record.is_some()
            && !self
                .pending_model_calls
                .iter()
                .any(|fact| fact.call_id == call_id)
        {
            event_tx.publish(RuntimeStreamEvent::Turn(model_call_activity(
                &self.scoped_effect_controller,
                &self.turn_id,
                &call_id,
            )));
        }
        let mut result = result;
        if let (Ok(response), Some(record)) = (&mut result, &call_record) {
            response.usage = record
                .usage()
                .map_err(|overflow| self.usage_overflow(overflow))?;
        }
        self.model_call_address = Some(crate::plugin::ModelCallAddress {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            call_id: call_id.clone(),
            attempt: observed_attempt,
        });
        self.record_unknown_failed_call(machine, &result, call_record.as_ref())?;
        self.record_llm_outcome(&result, call_record.as_ref());
        // Whether the hooks run at all is the plan recorded with the
        // completion (ADR 0105 §1), never the response hooks installed now:
        // a restore after a hook was added or removed derives as its first
        // execution would have.
        let result = match result {
            // The response hooks' derivation finishes the stream first, for
            // the states it hands them.
            Ok(response) if !response_plan.callbacks.is_empty() => match self
                .invoke_assistant_response_hooks_effect(
                    machine,
                    id,
                    response,
                    response_plan,
                    capsule,
                    event_tx,
                )
                .await
            {
                Ok(response) => Ok(response),
                Err(err) => {
                    self.fail_or_abort_runtime_effect_controller(machine, err)?;
                    return Ok(());
                }
            },
            result => {
                if let Err(err) = self.finish_recorded_stream(&capsule).await {
                    self.fail_or_abort_runtime_effect_controller(machine, err)?;
                    return Ok(());
                }
                result
            }
        };
        self.answer_llm_call(
            machine,
            id,
            result,
            text_streamed,
            reasoning_published,
            event_tx,
        )
    }

    /// Settle model call `id`, which an immediate cancel stopped, with
    /// `answer`, its outcome, without deriving it: its record is recorded,
    /// ahead of the turn's terminal, its stream is finished for its recorded
    /// reason, a cleanup whose failure is logged and never holds the stop
    /// back, and an unsuccessful outcome answers the machine. A completion
    /// that raced the cancel is left unanswered: the stop wins, and no
    /// response hook runs for it.
    pub(super) async fn settle_cancelled_llm_answer(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        answer: crate::runtime::durable::session::ModelAnswer,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let crate::runtime::durable::session::ModelAnswer {
            attempt,
            result,
            text_streamed,
            call_id,
            stream,
        } = answer;
        self.model_attempt = attempt;
        let call_record = self.recorded_model_call(&call_id).await?;
        if call_record.is_some()
            && !self
                .pending_model_calls
                .iter()
                .any(|fact| fact.call_id == call_id)
        {
            event_tx.publish(RuntimeStreamEvent::Turn(model_call_activity(
                &self.scoped_effect_controller,
                &self.turn_id,
                &call_id,
            )));
        }
        let mut result = result;
        if let (Ok(response), Some(record)) = (&mut result, &call_record) {
            response.usage = record
                .usage()
                .map_err(|overflow| self.usage_overflow(overflow))?;
        }
        self.model_call_address = Some(crate::plugin::ModelCallAddress {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            call_id: call_id.clone(),
            attempt: stream.attempt,
        });
        self.record_unknown_failed_call(machine, &result, call_record.as_ref())?;
        // A completion the stop drops still owes the usage of a tail that
        // drains: the turn counts it unknown until the drain settles it.
        if let Ok(response) = &result {
            machine
                .record_unanswered_call_usage(&response.usage)
                .map_err(|overflow| self.usage_overflow(overflow))?;
        }
        self.record_llm_outcome(&result, call_record.as_ref());
        if let Err(error) = self.finish_recorded_stream(&stream.capsule).await {
            tracing::warn!(
                session_id = %self.session_id,
                error = %error.message,
                "the cancelled model call's stream finish failed"
            );
        }
        match result {
            Ok(_) => Ok(()),
            Err(error) => self.answer_llm_call(
                machine,
                id,
                Err(error),
                text_streamed,
                stream.reasoning_published,
                event_tx,
            ),
        }
    }

    /// Record the outcome of the machine's model call on the turn: the
    /// failure evidence of an unsuccessful one, then the call's sealed
    /// record, published on the call's own lane ([`model_call_activity`]).
    fn record_llm_outcome(
        &mut self,
        result: &Result<LlmResponse, crate::LlmCallError>,
        call_record: Option<&crate::LlmCallRecord>,
    ) {
        if let Some(record) = call_record {
            self.sealed_attempt_count = self
                .sealed_attempt_count
                .saturating_add(record.attempts.len());
            if let Err(error) = result
                && self.failure_evidence.len() < self.sealed_attempt_count
                && let Some(evidence) = crate::TurnFailureEvidence::from_llm_failure(
                    error,
                    record,
                    self.trace
                        .runtime()
                        .evidence_limits()
                        .failure_partial_output_bytes(),
                )
            {
                self.failure_evidence.push(evidence);
            }
        }
    }

    /// Answer the machine's model call `id` with `result`, the response it
    /// reads.
    fn answer_llm_call(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        result: Result<LlmResponse, crate::LlmCallError>,
        text_streamed: bool,
        reasoning_published: Vec<crate::llm::types::StreamBlockIdentity>,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        if let Ok(response) = &result
            && !text_streamed
        {
            let prose_projector = self.session.plugins().assistant_prose_projector();
            let invocation = self
                .turn_effect_invocation(machine, id, RuntimeEffectKind::LlmCall)
                .map_err(RuntimeEffectControllerError::into_runtime_error)?;
            let stream_key = super::abandoned_stream::model_stream_key(
                invocation.effect_replay_key(),
                self.model_attempt,
            );
            emit_semantic_response_parts(
                event_tx,
                &mut self.turn_observations,
                &stream_key,
                response,
                prose_projector.as_deref(),
                &ReasoningPublicationState::from_published_blocks(reasoning_published),
            );
        }
        self.handle_machine_response(
            machine,
            Response::LlmComplete {
                id,
                result,
                text_streamed,
            },
        )?;
        Ok(())
    }

    pub(super) async fn handle_checkpoint_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        checkpoint: CheckpointKind,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let protocol_iteration = machine.protocol_iteration();
        let result = self
            .invoke_turn_checkpoint_effect(machine, id, checkpoint, event_tx)
            .await;
        match result {
            Ok(delivery) => {
                let committed_user_messages = delivery.committed_user_messages.clone();
                self.handle_machine_response(machine, Response::Checkpoint { id, delivery })?;
                self.turn_observations.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: None,
                        event: TurnEvent::CheckpointRecorded { protocol_iteration },
                    },
                );
                if let Some(mut admitted) = self.pending_checkpoint_turn_inputs.take() {
                    admitted.record_checkpoint_applications(
                        &self.turn_id,
                        checkpoint,
                        &committed_user_messages,
                    );
                    let applications = admitted.applications.clone();
                    let accepted_turn_inputs = admitted.accepted_turn_inputs();
                    self.pending_turn_inputs.push(admitted);
                    send_turn_input_applications(
                        event_tx,
                        &mut self.turn_observations,
                        applications,
                    );
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
                }
            }
            Err(err) => {
                // A failed checkpoint delivers nothing. What it admitted was
                // never bound: it stays open session mail.
                self.pending_checkpoint_turn_inputs = None;
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
            }
        }
        Ok(())
    }

    pub(super) async fn handle_execution_environment_sync_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let crate::runtime::effect::ServedExecutionEnvironmentSync {
            prelude,
            result,
            tool_surface,
        } = match self
            .invoke_turn_execution_environment_sync_effect(machine, id, event_tx)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        // The journal recorded the prelude by digest; live or replayed, it
        // is read back from the store and never re-derived (FIG-5133). A
        // prelude that cannot be read ends the attempt rather than the turn:
        // the journal already holds what followed the sync, so settling the
        // turn here would diverge from it. A live fault is retried; a prelude
        // that is gone or not the recorded one refuses the run, typed.
        let prelude = prelude
            .read(self.host.core.durability.turn_prelude_store.as_ref())
            .await
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let store = self.session.history_store().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ArtifactMissing,
                "the recorded turn prelude has no history store",
            )
        })?;
        // The prelude names the window the driver's own prelude names, whose
        // base the driver holds: it is read again only for another window.
        let (base, history, context) = crate::runtime::durable::head::restore_prelude(
            &prelude,
            Some((&self.prelude.history, &self.prelude_base_messages)),
            &store,
        )
        .await
        .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        machine.adopt_committed_messages(history);
        machine.adopt_prepared_messages(context.messages.clone());
        self.prelude_base_messages = base;
        self.prepared_context = context;
        self.prelude = prelude;
        // The surface the sync recorded is the one the iteration's tool calls
        // resolve against, whether the sync ran here or was served from the
        // journal (FIG-3672 P7b). Only a sync that built an environment
        // recorded one.
        if result.is_ok() {
            let tool_surface = Arc::new(tool_surface);
            self.session
                .install_recorded_tool_surface(Arc::clone(&tool_surface))
                // The install validates the recorded catalog as the sync's
                // step body did, so its failures are classified the same way
                // (FIG-4651): a live cause aborts the attempt under its own
                // code, and only a deterministic catalog defect ends the turn
                // as one.
                .map_err(|error| {
                    let message =
                        format!("the recorded tool surface could not be installed: {error}");
                    match super::tool_catalog::SyncFailure::of_plugin_error(
                        crate::sansio::ExecutionEnvironmentSyncFailureKind::ToolSurface,
                        error,
                    ) {
                        super::tool_catalog::SyncFailure::Live(fault) => fault,
                        super::tool_catalog::SyncFailure::Recorded(_) => RuntimeError::new(
                            RuntimeErrorCode::ToolCatalogResolutionFailed,
                            message,
                        ),
                    }
                })?;
            // The turn's next phase records the surface its model calls
            // offer, which a resume reinstalls instead of the live one.
            self.run_config.install_surface(tool_surface);
            self.surface_installed = true;
        }
        self.handle_machine_response(machine, Response::ExecutionEnvironmentSynced { id, result })?;
        Ok(())
    }

    pub(super) async fn handle_exec_code_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        language: String,
        code: String,
        event_tx: &TurnObserver,
    ) -> Result<CellExit, RuntimeError> {
        // The model response that asked for the cell, while the turn holds
        // it for the cell's first save: nothing of the cell reaches the
        // host's stream or trace before a commit holds it (FIG-6000).
        let held = self.held_response.take();
        let cell_tx = &HeldObservations {
            inner: event_tx,
            held: held.as_ref(),
        };
        let code_correlation_id = TurnActivityId::new(format!("code:{id:?}"));
        let iteration = machine.protocol_iteration();
        self.answered_cell_close = None;
        let invocation = match self.turn_effect_invocation(machine, id, RuntimeEffectKind::ExecCode)
        {
            Ok(invocation) => invocation,
            Err(err) => {
                let message = err.to_string();
                // A cell with no invocation has no identity and no span: its
                // failure is the turn's, an event of the turn's span.
                self.cell_start = None;
                if self.trace.is_observed() {
                    let started_at_ms = self.host.core.tracing.clock().timestamp_ms();
                    let context = self.trace_context(iteration);
                    let standing = self
                        .trace
                        .selected(lash_trace::TraceSpanKind::Agent, &context.ids());
                    let content = standing.content();
                    let event = lash_trace::TraceEvent::ExecCodeFailed {
                        trailing_discarded: 0,
                        code: content.capture(lash_trace::ContentCategory::Code, || code.clone()),
                        language: language.clone(),
                        started_at_ms,
                        duration_ms: None,
                        reason: crate::ExecCodeFailureReason::Session,
                        error: content
                            .capture(lash_trace::ContentCategory::CodeOutput, || message.clone()),
                    };
                    self.emit_cell_trace(held.as_ref(), standing, false, move || (context, event));
                }
                self.turn_observations.observe(
                    cell_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockStarted {
                            language: language.clone(),
                            code: code.clone(),
                            graph_key: None,
                        },
                    },
                );
                self.turn_observations.observe(
                    cell_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockCompleted {
                            language: language.clone(),
                            prints: Some(Vec::new()),
                            prints_retained: None,
                            result: crate::CellOutcome::Failed(crate::CellFailure::new(
                                crate::CellFailureKind::Host,
                                message,
                            )),
                            duration_ms: None,
                            tool_call_ids: Some(Vec::new()),
                            graph_key: None,
                        },
                    },
                );
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(CellExit::Answered);
            }
        };
        let cell_graph_key = foreground_effect_graph_key(&invocation);
        let graph_key = Some(cell_graph_key.clone());
        let cell_key = invocation.effect_replay_key().to_string();
        // The cell's span starts at its first start, which the phase that
        // holds the cell retains, whichever execution this is (§4.5).
        self.begin_cell(id, &language);
        let first_started_at_ms = self
            .cell_start
            .as_ref()
            .map_or(0, |start| start.first_started_at_ms);
        let cell_trace = CellTrace {
            graph_key: cell_graph_key,
            language: language.clone(),
            first_started_at_ms,
            context: {
                let mut context = self.trace_context(iteration);
                context.cell = graph_key.clone();
                context
            },
        };
        if self.trace.is_observed() {
            let standing = cell_trace
                .standing(&self.trace)
                .selected(lash_trace::TraceSpanKind::Cell, &cell_trace.context.ids());
            let event = lash_trace::TraceEvent::ExecCodeStarted {
                language: language.clone(),
                code_chars: code.chars().count(),
            };
            let context = cell_trace.context.clone();
            self.emit_cell_trace(held.as_ref(), standing, true, move || (context, event));
        }
        let discarded_bytes = machine.cell_trailing_discarded().map_err(|error| {
            let mut failure =
                RuntimeError::new(RuntimeErrorCode::RuntimeStoreCorrupt, error.to_string());
            failure.cause = Some(crate::RuntimeErrorCause::StoredDataCorrupt {
                corruption: Box::new(error),
            });
            failure
        })?;
        // The cell's own observation lane: every CodeBlock* event this driver
        // publishes for the cell sequences under the cell's replay key.
        let mut code_observations = crate::engine::ObservationCursor::new(
            crate::engine::ReplayKey::new(format!("{cell_key}:code")),
        );
        code_observations.observe(
            cell_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(code_correlation_id.clone()),
                event: TurnEvent::CodeBlockStarted {
                    language: language.clone(),
                    code: code.clone(),
                    graph_key: graph_key.clone(),
                },
            },
        );
        // The observed duration is measured around the journaled invocation:
        // the recorded `ExecResponse` carries no wall-clock fields, so the
        // Completed activity takes the live window here.
        let cell_started = self.host.core.clock.now();
        self.answered_cell_end = None;
        drop(self.cell_end.take());
        self.held_response.clone_from(&held);
        let invoked = self
            .invoke_turn_exec_effect(machine, invocation, code.clone(), event_tx)
            .await;
        self.held_response = None;
        // What the cell left of its end, whether it answers with its result
        // or with a failure met after it ended.
        self.answered_cell_end = self.cell_end.take().map(|ended| (id, ended));
        let result = match invoked {
            Ok(result) => result,
            Err(err) => {
                let message = err.to_string();
                // No executor response was available to observe; its timing,
                // prints and calls are unavailable.
                code_observations.observe(
                    cell_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockCompleted {
                            language: language.clone(),
                            prints: None,
                            prints_retained: None,
                            result: crate::CellOutcome::Failed(crate::CellFailure::new(
                                crate::CellFailureKind::Host,
                                message,
                            )),
                            duration_ms: None,
                            tool_call_ids: None,
                            graph_key: graph_key.clone(),
                        },
                    },
                );
                // A turn cancel never answers a cell: it stops the turn before
                // the cell's result reaches the machine, so a cell that
                // aborted is discarded.
                if let Some(code_executor) = self.session.plugins().code_executor() {
                    code_executor
                        .settle_code_execution(crate::plugin::CodeExecutionOutcome::Discarded)
                        .await
                        .map_err(|error| {
                            RuntimeError::new(
                                RuntimeErrorCode::ExecutionStateCaptureFailed,
                                error.to_string(),
                            )
                        })?;
                }
                // A live fault or a session retirement aborts the turn.
                if Self::aborts_turn(&err) {
                    return Err(err.into_runtime_error());
                }
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(CellExit::Answered);
            }
        };
        // A cell whose calls wait only on rows (a parked call, a retry's due
        // time, a timer) stopped on its committed snapshot: it has no answer
        // yet, and the turn suspends until the earliest due. Its in-memory
        // run is discarded; the activation that resumes the turn resumes the
        // cell from that snapshot.
        if result.as_ref().is_ok_and(|output| output.suspended) {
            if let Some(code_executor) = self.session.plugins().code_executor() {
                code_executor
                    .settle_code_execution(crate::plugin::CodeExecutionOutcome::Discarded)
                    .await
                    .map_err(|error| {
                        RuntimeError::new(
                            RuntimeErrorCode::ExecutionStateCaptureFailed,
                            error.to_string(),
                        )
                    })?;
            }
            return Ok(CellExit::Suspended);
        }
        let cell_duration_ms = self
            .host
            .core
            .clock
            .now()
            .saturating_duration_since(cell_started)
            .as_millis() as u64;
        if let Ok(output) = &result {
            self.answered_cell_calls = self.read_cell_calls(machine, id).await?;
            // Bodies are transient: the original ledger nodes are retained
            // by the consuming commit, never copied into the turn.
            self.answered_cell_calls.records = output.tool_calls.clone();
        }
        match &result {
            Ok(output) => {
                code_observations.observe(
                    cell_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockCompleted {
                            language: language.clone(),
                            prints: Some(output.prints.clone()),
                            prints_retained: output.prints_retained.clone(),
                            result: output.result.clone(),
                            duration_ms: Some(cell_duration_ms),
                            tool_call_ids: Some(
                                output
                                    .tool_calls
                                    .iter()
                                    .map(|record| record.call_id.clone())
                                    .collect(),
                            ),
                            graph_key: graph_key.clone(),
                        },
                    },
                );
            }
            Err(error) => {
                code_observations.observe(
                    cell_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockCompleted {
                            language: language.clone(),
                            prints: None,
                            prints_retained: None,
                            result: crate::CellOutcome::Failed(crate::CellFailure::new(
                                crate::CellFailureKind::Host,
                                error.message.clone(),
                            )),
                            duration_ms: Some(cell_duration_ms),
                            tool_call_ids: None,
                            graph_key: graph_key.clone(),
                        },
                    },
                );
            }
        }
        // The cell answered: its span closes once the commit that consumes
        // the answer is acknowledged, from its first start to this end (P1).
        self.cell_start = None;
        if self.trace.is_observed() {
            let ended_at_ms = self.host.core.tracing.clock().timestamp_ms();
            let duration_ms = ended_at_ms.saturating_sub(first_started_at_ms);
            let committed = self
                .host
                .core
                .tracing
                .unreplayed(self.trace.scope().cloned());
            let standing = cell_trace
                .standing(&committed)
                .selected(lash_trace::TraceSpanKind::Cell, &cell_trace.context.ids());
            let content = standing.content();
            if standing.is_observed() {
                let event = match &result {
                    Ok(output) => {
                        let observations_text = content
                            .capture(lash_trace::ContentCategory::CodeOutput, || {
                                join_prints(&output.prints)
                            });
                        let observation_projections = output
                            .prints
                            .iter()
                            .map(|observation| observation.projection.clone())
                            .collect::<Vec<_>>();
                        if !observation_projections.is_empty() {
                            let projections = observation_projections.clone();
                            let context = cell_trace.context.clone();
                            self.emit_cell_trace(
                                held.as_ref(),
                                cell_trace.standing(&self.trace),
                                false,
                                move || {
                                    (
                                        context,
                                        lash_trace::TraceEvent::ObservationProjection {
                                            projections,
                                        },
                                    )
                                },
                            );
                        }
                        lash_trace::TraceEvent::ExecCodeCompleted {
                            trailing_discarded: discarded_bytes,
                            code: content
                                .capture(lash_trace::ContentCategory::Code, || code.clone()),
                            language: language.clone(),
                            started_at_ms: first_started_at_ms,
                            duration_ms,
                            output: observations_text,
                            output_chars: output
                                .prints
                                .iter()
                                .map(|p| p.text.chars().count())
                                .sum::<usize>()
                                + output.prints.len().saturating_sub(1),
                            observation_count: output.prints.len(),
                            observation_projections,
                            error: output
                                .error()
                                .map(|failure| crate::trace::trace_cell_failure(failure, content)),
                            terminal_finish: content
                                .capture(lash_trace::ContentCategory::CodeOutput, || {
                                    output.finish_value().cloned()
                                }),
                            tool_calls: output
                                .tool_calls
                                .iter()
                                .map(|record| lash_trace::TraceExecToolCall {
                                    call_id: record.call_id.clone(),
                                    name: record.tool.clone(),
                                    status: match record.output.status() {
                                        lash_protocol_types::ToolCallStatus::Success => {
                                            lash_trace::TraceToolCallStatus::Success
                                        }
                                        lash_protocol_types::ToolCallStatus::Failure => {
                                            lash_trace::TraceToolCallStatus::Failure
                                        }
                                        lash_protocol_types::ToolCallStatus::Cancelled => {
                                            lash_trace::TraceToolCallStatus::Cancelled
                                        }
                                    },
                                })
                                .collect(),
                        }
                    }
                    Err(error) => lash_trace::TraceEvent::ExecCodeFailed {
                        trailing_discarded: discarded_bytes,
                        code: content.capture(lash_trace::ContentCategory::Code, || code.clone()),
                        language: language.clone(),
                        started_at_ms: first_started_at_ms,
                        duration_ms: Some(duration_ms),
                        reason: error.reason,
                        error: content.capture(lash_trace::ContentCategory::CodeOutput, || {
                            error.message.clone()
                        }),
                    },
                };
                self.answered_cell_close =
                    Some((id, cell_trace.close(standing, ended_at_ms, event)));
            }
        }
        // A cancelled tool call ended the cell as an uncatchable host terminal,
        // so the execution settles as cancelled even without a host cancel
        // request. The protocol then reads the cancelled record off the call
        // ledger and finishes the turn cancelled itself.
        let tool_call_cancelled = result.as_ref().ok().is_some_and(|output| {
            output.tool_calls.iter().any(|record| {
                record.output.status() == lash_protocol_types::ToolCallStatus::Cancelled
            })
        });
        if let Some(code_executor) = self.session.plugins().code_executor() {
            code_executor
                .settle_code_execution(if tool_call_cancelled {
                    crate::plugin::CodeExecutionOutcome::Cancelled
                } else {
                    crate::plugin::CodeExecutionOutcome::Accepted
                })
                .await
                .map_err(|error| {
                    RuntimeError::new(
                        RuntimeErrorCode::ExecutionStateCaptureFailed,
                        error.to_string(),
                    )
                })?;
        }
        self.handle_machine_response(machine, Response::ExecResult { id, result })?;
        Ok(CellExit::Answered)
    }
}

/// The host stream a cell publishes on: once a commit holds the model
/// response that asked for the cell, now unless its turn still holds it
/// (FIG-6000).
struct HeldObservations<'a> {
    inner: &'a TurnObserver,
    held: Option<&'a crate::HeldResponse>,
}

impl crate::engine::ObservationSink for HeldObservations<'_> {
    fn observe(&self, observation: crate::engine::ShiftObservation) {
        match self.held {
            Some(held) => {
                let inner = self.inner.clone();
                held.after_commit(Box::new(move || inner.observe(observation)));
            }
            None => self.inner.observe(observation),
        }
    }
}

/// What a cell's trace records name: its span's key, language and first
/// start, and the context of its records.
struct CellTrace {
    graph_key: String,
    language: String,
    first_started_at_ms: u64,
    context: lash_trace::TraceContext,
}

impl CellTrace {
    /// Where code standing at `trace` observes the cell: its span, under the
    /// turn's, or the turn's own standing when the turn has no observed span.
    fn standing(&self, trace: &crate::trace::TraceStanding) -> crate::trace::TraceStanding {
        trace
            .derived(
                lash_trace::TraceSpanKind::Cell,
                &lash_trace::SpanKey::Cell {
                    graph_key: &self.graph_key,
                },
                self.first_started_at_ms,
            )
            .unwrap_or_else(|| trace.clone())
    }

    /// The close of the cell's span with `event` at `ended_at_ms`, for the
    /// commit that consumes the cell's answer, or the terminal that discards
    /// it, to publish once it is acknowledged. The commit, not this
    /// execution, gives the right to publish it (P1).
    fn close(
        &self,
        standing: crate::trace::TraceStanding,
        ended_at_ms: u64,
        event: lash_trace::TraceEvent,
    ) -> crate::runtime::durable::session::CellClose {
        let context = self.context.clone();
        crate::runtime::durable::session::CellClose::new(move || {
            standing.close_scope(ended_at_ms, || (context, event));
        })
    }
}

impl RuntimeTurnDriver<'_> {
    /// Emits `record` at `standing` (a cell's span, or its turn's), as the
    /// span's opening record when `opens`, else as its event: once a commit
    /// holds the model response that asked for the cell, now unless `held`
    /// still holds it (FIG-6000).
    fn emit_cell_trace(
        &self,
        held: Option<&crate::HeldResponse>,
        standing: crate::trace::TraceStanding,
        opens: bool,
        record: impl FnOnce() -> (lash_trace::TraceContext, lash_trace::TraceEvent) + Send + 'static,
    ) {
        if !self.trace.is_observed() {
            return;
        }
        let emit = move || {
            if opens {
                standing.open_scope(record);
            } else {
                standing.observe(record);
            }
        };
        match held {
            Some(held) => held.after_commit(Box::new(emit)),
            None => emit(),
        }
    }

    /// Retain the first start of the cell of effect `id` in `language`, now,
    /// unless the turn retains one for it already.
    pub(super) fn begin_cell(&mut self, id: crate::EffectId, language: &str) {
        if self
            .cell_start
            .as_ref()
            .is_some_and(|start| start.effect == id)
        {
            return;
        }
        self.cell_start = Some(crate::runtime::durable::session::CellStart {
            effect: id,
            language: language.to_owned(),
            first_started_at_ms: self.host.core.tracing.clock().timestamp_ms(),
        });
    }

    /// The effect and close of the cell the turn's phase holds, which never
    /// answered, for the terminal that discards it: published at the clock
    /// of the owner whose commit wrote that terminal. `None` when no
    /// anchored cell is open, or nothing observes it.
    pub(super) fn discarded_cell(
        &mut self,
        machine: &TurnMachine,
    ) -> Option<(crate::EffectId, crate::runtime::durable::session::CellClose)> {
        let start = self.cell_start.take()?;
        let effect = start.effect;
        if !self.trace.is_observed() {
            return None;
        }
        let invocation = self
            .turn_effect_invocation(machine, start.effect, RuntimeEffectKind::ExecCode)
            .ok()?;
        let graph_key = foreground_effect_graph_key(&invocation);
        let mut context = self.trace_context(machine.protocol_iteration());
        context.cell = Some(graph_key.clone());
        let cell = CellTrace {
            graph_key,
            language: start.language,
            first_started_at_ms: start.first_started_at_ms,
            context,
        };
        let committed = self
            .host
            .core
            .tracing
            .unreplayed(self.trace.scope().cloned());
        let standing = cell
            .standing(&committed)
            .selected(lash_trace::TraceSpanKind::Cell, &cell.context.ids());
        // The waiting execution is retained in the turn checkpoint and is
        // available even when this owner never ran the cell's body.
        let code = standing
            .content()
            .capture(lash_trace::ContentCategory::Code, || {
                machine
                    .waiting_exec()
                    .map(|(_, code, _)| code.to_owned())
                    .unwrap_or_default()
            });
        let trailing_discarded = machine.cell_trailing_discarded().unwrap_or(0);
        let clock = Arc::clone(self.host.core.tracing.clock());
        let CellTrace {
            language,
            first_started_at_ms,
            context,
            ..
        } = cell;
        let close = crate::runtime::durable::session::CellClose::new(move || {
            let ended_at_ms = clock.timestamp_ms();
            standing.close_scope(ended_at_ms, || {
                (
                    context,
                    lash_trace::TraceEvent::ExecCodeDiscarded {
                        trailing_discarded,
                        code,
                        language,
                        started_at_ms: first_started_at_ms,
                        duration_ms: ended_at_ms.saturating_sub(first_started_at_ms),
                    },
                )
            });
        });
        Some((effect, close))
    }
}

fn join_prints(prints: &[crate::CellPrint]) -> String {
    prints
        .iter()
        .map(|print| print.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn foreground_effect_graph_key(invocation: &RuntimeEffectInvocation) -> String {
    invocation.address().graph_key()
}

pub(super) fn foreground_exec_graph_key(invocation: &RuntimeInvocation) -> Option<String> {
    invocation
        .effect_address()
        .map(crate::EffectAddress::graph_key)
}

/// Model call `record`, sealed, as the activity of turn `turn` admitted
/// under `controller`'s scope. It is the one observation of the call's own
/// lane, keyed by its call id: every owner that records it (the one that
/// sent the call, one that derived or settled its receipt, or a node that
/// ended its turn from the rows alone) publishes it under one id, which the
/// live feed publishes once (FIG-3753, FIG-6274).
pub(in crate::runtime) fn model_call_activity(
    controller: &ActorContext,
    turn: &crate::TurnId,
    call_id: &crate::LlmCallId,
) -> crate::TurnActivity {
    let lane = crate::runtime::turn_loop::turn_observation_cursor(
        controller,
        turn,
        &format!("model-call:{}", call_id.0),
    );
    crate::TurnActivity {
        id: TurnActivityId::observed(lane.key(), 0),
        correlation_id: TurnActivityId::new(call_id.0.clone()),
        event: TurnEvent::ModelCallRecorded {
            call_id: call_id.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_exec_graph_key_uses_runtime_invocation_identity() {
        let effect = RuntimeInvocation::effect(
            EffectAddress::new(ExecutionScope::turn("session-1", "turn-1"), "replay-key")
                .expect("valid foreground exec address"),
            RuntimeAttribution::for_turn("session-1", "turn-1", 2, 3),
            "effect-7",
        );
        assert_eq!(
            foreground_exec_graph_key(&effect).as_deref(),
            Some(
                "effect:{\"version\":2,\"kind\":\"turn\",\"session_id\":\"session-1\",\"execution_id\":\"turn-1\"}:\"replay-key\""
            )
        );

        let process = RuntimeInvocation {
            attribution: RuntimeAttribution::for_turn("session-1", "turn-1", 2, 3),
            subject: crate::RuntimeSubject::Process {
                process_id: crate::ProcessId::fixture("process-1"),
            },
            caused_by: None,
            replay: None,
        };
        assert_eq!(foreground_exec_graph_key(&process), None);
    }
}
