//! The prepare phase: refresh the resident graph, materialize the admitted
//! input, normalize its items, and run the context transform that produces the
//! message sequence the execute phase drives.

use super::*;

/// Everything the prepare phase needs to turn an admitted [`TurnInput`] into
/// a driven physical turn.
///
/// The accept phase builds one of these and the prepare phase consumes it; the
/// fields are the phase's inputs in the order the phase reads them.
pub(in crate::runtime) struct TurnPrepareContext<'sinks, 'run> {
    pub(in crate::runtime) input: TurnInput,
    /// Protocol turn options this physical turn runs under beyond its root's
    /// recorded view: a follow-on's recorded options. A root's own options
    /// come from its resolved run spec, never from here.
    pub(in crate::runtime) protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    pub(in crate::runtime) sinks: TurnSinks<'sinks>,
    pub(in crate::runtime) scoped_effect_controller: ScopedEffectController<'run>,
    pub(in crate::runtime) local_stop: LocalTurnStop,
    pub(in crate::runtime) admissions: LogicalTurnAdmissions,
    pub(in crate::runtime) materialize_initial_admissions: bool,
    pub(in crate::runtime) drive_fence: Option<&'sinks DriveFence>,
}

impl LashRuntime {
    /// Bring the resident session up to the durable head: reload invalidated
    /// resident state, then the graph.
    ///
    /// Either adoption drops the running root's resident evidence (FIG-1875:
    /// the head wins for every fact it carries) — but it also reverts the
    /// root's recorded execution view, which the head does not carry. Whether
    /// this refresh ran then changes what a later commit in the same root
    /// writes, which breaks redrive determinism (FIG-3877): a replay that
    /// skips the refresh hashes different commit content than the attempt
    /// that ran it. Re-installing the captured record afterwards makes the
    /// outcome identical either way; between roots the record is absent and
    /// nothing is restored.
    pub(in crate::runtime) async fn refresh_resident_head(&mut self) -> Result<(), RuntimeError> {
        let resolved_run = self.state.authority.resolved_run.clone();
        self.reload_invalidated_resident_session().await?;
        self.refresh_session_graph_from_store()
            .await
            .map_err(session_head_refresh_error)?;
        if let Some(resolved) = resolved_run {
            self.install_resolved_run(&resolved);
        }
        Ok(())
    }

    /// The physical turn's index: the one its admission recorded, else the
    /// resident head's next.
    fn physical_turn_index(&self, admitted_turn_index: Option<usize>) -> usize {
        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
        admitted_turn_index.unwrap_or(self.state.turn_index + 1)
    }

    #[expect(
        clippy::expect_used,
        reason = "the trace turn id is bound before validation"
    )]
    pub(super) async fn stream_turn_inner(
        &mut self,
        context: TurnPrepareContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let TurnPrepareContext {
            mut input,
            protocol_turn_options,
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            local_stop,
            mut admissions,
            materialize_initial_admissions,
            drive_fence,
        } = context;
        // A direct turn's admission already adopted the head it was admitted
        // on and recorded its index (FIG-3682): re-reading the live head here
        // would undo that on a replay after the turn's own commit.
        let admitted_turn_index = self.admitted_turn_index.take();
        if admitted_turn_index.is_none() {
            self.refresh_resident_head().await?;
        }
        // `load_session` refreshes the committed graph/head, checkpoint,
        // config, frames, and token ledger. It does not cover pending turn
        // inputs, queued work, or trigger deliveries; those remain external
        // ingress and are picked up by their fenced admission paths.
        let input_trace_turn_id = input.trace_turn_id.clone();
        let pending_turn_input = materialize_initial_admissions
            .then(|| admissions.turn_inputs.first())
            .flatten()
            .map(crate::AdmittedTurnInputs::materialize_turn_input);
        if let Some(work) = pending_turn_input.as_ref()
            && input.items.is_empty()
        {
            let turn_context = input.turn_context.clone();
            input = work.clone();
            // Retain host controls installed on the initially materialized input. The admission
            // is rematerialized here to refresh durable payloads, not to erase live run policy.
            input.turn_context = turn_context;
            if input.trace_turn_id.is_none() {
                input.trace_turn_id = input_trace_turn_id;
            }
        }
        let previous_prompt_usage = self.state.last_prompt_usage.clone();
        let normalized = match self.normalize_input_items(&input.items).await {
            Ok(items) => items,
            Err(e) => {
                self.state.last_prompt_usage = None;
                let mut recorded_assembly = RecordedTurnAssembly::default();
                let trace_turn_id = input
                    .trace_turn_id
                    .clone()
                    .expect("turn id is bound from the execution scope before validation");
                hold_terminal_sequence(
                    &mut recorded_assembly,
                    observer,
                    &mut turn_observation_cursor(
                        &scoped_effect_controller,
                        &trace_turn_id,
                        "terminal",
                    ),
                    Some(TerminalDiagnostic {
                        kind: TerminalDiagnosticKind::InputValidation,
                        code: Some(crate::TurnFailureCode::InvalidTurnInput.into()),
                        message: e,
                        retryable: Some(false),
                        activity: TerminalActivityTarget::ForTurn {
                            observer,
                            turn_id: &trace_turn_id,
                        },
                    }),
                    TurnStop::InvalidInput,
                );
                let turn_index = self.physical_turn_index(admitted_turn_index);
                let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
                let turn_control_binding =
                    turn_control_binding(turn_control_host.as_ref(), &scoped_effect_controller)
                        .await?;
                let turn_control_resolver = turn_control_binding.resolver();
                let turn_control = ActiveTurnControl::new(
                    turn_control_resolver,
                    TurnAddress::new(&self.state.session_id, &trace_turn_id),
                )
                .await?;
                let messages = crate::MessageSequence::from_base(
                    self.state
                        .read_model()
                        .map_err(|error| {
                            RuntimeError::new(
                                RuntimeErrorCode::ContextPrepareTurn,
                                error.to_string(),
                            )
                        })?
                        .messages,
                );
                let mut turn_pipeline = TurnBoundary::from_state_with_clock(
                    self.state.clone(),
                    Arc::clone(&self.host.core.clock),
                    self.state.turn_scope(&trace_turn_id),
                    self.host.core.durability.commit_budget,
                );
                turn_pipeline.apply_prepared_messages(&messages);
                return Box::pin(self.finish_turn(TurnCommitContext {
                    finish: TurnFinishInput {
                        turn_pipeline,
                        recorded_assembly,
                        new_messages: messages,
                        policy: self.state.effective_policy().clone(),
                        turn_index,
                        trace_turn_id,
                    },
                    admissions: &admissions,
                    scoped_effect_controller: &scoped_effect_controller,
                    honoured_cancel: None,
                    drive_fence,
                    turn_control: &turn_control,
                    observer,
                }))
                .await;
            }
        };
        let turn_index = self.physical_turn_index(admitted_turn_index);
        let trace_turn_id = input
            .trace_turn_id
            .clone()
            .expect("turn id is bound from the execution scope before normalization");
        if self.host.core.tracing.trace_sink.is_some() {
            let mut trace_metadata = std::collections::BTreeMap::new();
            trace_metadata.insert(
                "input_item_count".to_string(),
                serde_json::json!(normalized.len()),
            );
            // The config this physical turn runs under (FIG-3600 S6): the
            // root's recorded config, adopted on resident state at the
            // funnel's `ResolveTurnConfig` step.
            trace_metadata.insert(
                "provider_id".to_string(),
                serde_json::json!(self.state.policy.provider_id),
            );
            trace_metadata.insert(
                "model".to_string(),
                serde_json::json!(self.state.policy.model.id),
            );
            trace_metadata.insert(
                "config_revision".to_string(),
                serde_json::json!(self.state.config_revision),
            );
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                lash_trace::TraceContext::default()
                    .for_session(self.state.session_id.clone())
                    .for_turn_index(turn_index)
                    .for_turn(trace_turn_id.clone()),
                lash_trace::TraceEvent::TurnStarted {
                    metadata: trace_metadata,
                },
                self.host.core.clock.as_ref(),
            );
        }

        let mut turn_delta = Vec::new();
        let initial_turn_causes: Vec<_> = admissions
            .queued
            .iter()
            .filter(|_| materialize_initial_admissions)
            .flat_map(|queued| queued.materialize_queued_checkpoint_work().turn_causes)
            .collect();
        turn_delta.extend(
            initial_turn_causes
                .iter()
                .map(crate::TurnCause::to_event_message),
        );

        let turn_input_id = admissions
            .turn_inputs
            .iter()
            .flat_map(|admitted| admitted.inputs.iter().map(|input| input.input_id.clone()))
            .next();
        let user_id = turn_input_id
            .as_deref()
            .map(crate::runtime::ingress_message_id)
            .unwrap_or_else(|| format!("m_turn_{trace_turn_id}_input"));
        let mut user_parts: Vec<Part> = Vec::new();
        for item in normalized {
            match item {
                NormalizedItem::Text(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    user_parts.push(Part::text(
                        format!("{}.p{}", user_id, user_parts.len()),
                        text,
                        None,
                    ));
                }
                NormalizedItem::Attachment(source) => {
                    user_parts.push(Part::attachment_part(
                        format!("{}.p{}", user_id, user_parts.len()),
                        String::new(),
                        Some(crate::session_model::message::PartAttachment { source }),
                    ));
                }
            }
        }
        if user_parts.is_empty() && initial_turn_causes.is_empty() {
            user_parts.push(Part::text(format!("{}.p0", user_id), String::new(), None));
        }
        if !user_parts.is_empty() {
            reassign_part_ids(&user_id, &mut user_parts);
            turn_delta.push(Message {
                id: user_id.clone(),
                role: MessageRole::User,
                parts: shared_parts(user_parts),
                // Typed provenance, not a pinned id: a host that rendered its
                // own row for this turn recognizes the committed copy by
                // `turn_id` (FIG-972).
                origin: Some(crate::MessageOrigin::TurnInput {
                    turn_id: trace_turn_id.clone(),
                    input_id: turn_input_id.clone(),
                }),
            });
        }
        let mut initial_turn_input_applications = Vec::new();
        for admitted in &mut admissions.turn_inputs {
            admitted
                .record_initial_turn_application(&crate::TurnId::from(&trace_turn_id), &user_id);
            initial_turn_input_applications.extend(admitted.applications.iter().cloned());
        }
        if !initial_turn_input_applications.is_empty() {
            turn_observation_cursor(&scoped_effect_controller, &trace_turn_id, "prepare").observe(
                &observer.for_turn(&trace_turn_id),
                crate::engine::ObservedEvent::Activity {
                    correlation_id: None,
                    event: TurnEvent::QueuedInputAccepted {
                        applications: initial_turn_input_applications,
                    },
                },
            );
        }

        // Context pressure runs before the turn's graph-append draft exists: a
        // frame it opens commits on its own, and the turn is drafted over the
        // frame the turn then runs in (FIG-4110).
        let pressure = self
            .run_context_pressure(ContextPressureStep {
                trace_turn_id: &trace_turn_id,
                previous_prompt_usage: previous_prompt_usage.clone(),
                scoped_effect_controller: &scoped_effect_controller,
                drive_fence,
            })
            .await?;
        // After a frame opens, the old frame's usage is not the new frame's:
        // it stays unknown until the new frame's first provider response.
        let previous_prompt_usage = if pressure.opened_frame {
            None
        } else {
            previous_prompt_usage
        };

        // One graph-append draft per physical turn: prepare-turn hooks, the
        // turn driver's hooks, and finalize-turn hooks all record into it and
        // the turn boundary commits it with the turn.
        let turn_graph_appends = TurnGraphAppendDraft::from_resident_state(
            &self.state,
            Arc::clone(&self.host.core.clock),
        );
        for records in &pressure.turn_records {
            turn_graph_appends
                .record(&self.state.session_id, records)
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, err.to_string())
                })?;
        }
        let manager = self
            .runtime_session_services_for_turn(drive_fence, &turn_graph_appends)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let plugin_session = Arc::clone(
            self.session
                .as_ref()
                .ok_or_else(|| {
                    RuntimeError::new(
                        RuntimeErrorCode::ContextPrepareTurn,
                        "runtime session not available",
                    )
                })?
                .plugins(),
        );
        // The base window and the read view are read after the pressure step,
        // so the transforms and pruning see the frame the turn runs in.
        let base_read_model = self.state.read_model().map_err(|error| {
            RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, error.to_string())
        })?;
        let prepare_read_view = self.read_view().map_err(|error| {
            RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, error.to_string())
        })?;
        let base_messages = base_read_model.messages;
        let base_render_cache = base_read_model.prompt_render_cache;
        let turn_ctx = crate::TurnTransformContext {
            session_id: self.state.session_id.clone(),
            state: prepare_read_view,
            prompt_usage: previous_prompt_usage.clone(),
            max_context_tokens: Some(LashRuntime::max_context_tokens(self)),
            traces: manager.trace_emitter(),
            scoped_effect_controller: scoped_effect_controller.clone(),
            direct_completions: manager.direct_completion_client(
                RuntimeEffectControllerHandle::borrowed(scoped_effect_controller.clone()),
                Some(turn_phase_id(&trace_turn_id, "prepare-turn")),
            ),
        };
        self.mark_phase_begin(RuntimeTurnPhase::ContextTransform);
        let prepared_context = plugin_session
            .prepare_turn_context(
                &turn_ctx,
                crate::session_model::context::PreparedContext {
                    messages: crate::MessageSequence::from_base_and_delta(
                        base_messages,
                        turn_delta,
                    )
                    .with_base_render_cache(base_render_cache),
                    ..Default::default()
                },
                self.turn_phase_probe.clone(),
            )
            .await
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::ContextPrepareTurn))?;
        self.mark_phase_end(RuntimeTurnPhase::ContextTransform);
        // Release the read-view's graph clone before the rest of the turn
        // runs. Keeping it alive into the execute phase forces the
        // post-turn `append_active_read_delta` to deep-clone the session
        // graph (Arc::make_mut with refcount > 1).
        drop(turn_ctx);
        let messages = prepared_context.messages;
        if let Some(session) = self.session.as_mut() {
            session
                .set_context_overlay(
                    prepared_context.tool_providers,
                    prepared_context.prompt_contributions,
                )
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::SessionToolRegistry, err.to_string())
                })?;
        }

        self.state.last_prompt_usage = None;
        Box::pin(self.stream_prepared_turn_inner_with_graph_appends(
            PreparedTurnExecuteContext {
                turn: PreparedLogicalTurn {
                    messages,
                    previous_prompt_usage,
                    protocol_turn_options,
                    turn_context: input.turn_context.clone(),
                    initial_turn_causes,
                    trace_turn_id,
                    turn_index,
                },
                sinks: TurnSinks { observer },
                scoped_effect_controller,
                local_stop,
                initial_admissions: admissions,
                drive_fence,
            },
            turn_graph_appends,
        ))
        .await
    }

    pub async fn normalize_input_items(
        &self,
        items: &[InputItem],
    ) -> Result<Vec<NormalizedItem>, String> {
        normalize_input_items(
            items,
            self.host.core.durability.attachment_store.as_ref(),
            self.host.core.attachment_source_policy.as_ref(),
        )
        .await
    }
}
