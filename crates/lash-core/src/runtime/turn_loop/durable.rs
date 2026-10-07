//! A turn the session actor runs (ADR 0132 §4; L3, FIG-5172): everything
//! the turn holds in memory, prepared from committed state alone. The
//! session's head, the rows its run took and the configuration its run
//! recorded; nothing here commits, and the phase runner drives what it
//! returns.

use super::*;

/// A turn prepared from committed state: its driver, and the messages its
/// machine starts from.
pub(in crate::runtime) struct DurableTurn {
    pub(in crate::runtime) driver: Box<RuntimeTurnDriver<'static>>,
    pub(in crate::runtime) messages: crate::MessageSequence,
    /// Why the admitted input did not normalize: the turn ends at once,
    /// `InvalidInput`, without calling the model.
    pub(in crate::runtime) invalid_input: Option<String>,
}

impl LashRuntime {
    /// Prepare run `run` of this session for the session actor: install the
    /// run's recorded configuration, materialize its admitted input over the
    /// head, run the before-turn hooks and the context transform, and build
    /// the driver that answers the turn's effects under `controller`, the
    /// turn's own claimed context. `admissions` are the rows the run took.
    #[expect(
        clippy::expect_used,
        reason = "the runtime session is installed for the whole preparation"
    )]
    pub(in crate::runtime) async fn prepare_durable_turn(
        &mut self,
        controller: &ActorContext,
        run: &TurnId,
        mut admissions: LogicalTurnAdmissions,
        observer: &TurnObserver,
    ) -> Result<DurableTurn, RuntimeError> {
        // An admission never mixes run specs, so the head input's spec is the
        // run's; a run of queued work runs the default spec.
        let admitted_run_spec = admissions
            .turn_inputs
            .first()
            .and_then(|admitted| admitted.inputs.first())
            .and_then(|input| input.run_spec.clone());
        self.materialize_turn_session(controller).await?;
        self.resolve_turn_config(controller, run, admitted_run_spec.as_ref(), None)
            .await?;
        Self::emit_physical_turn_start(
            observer,
            controller,
            run,
            &admissions,
            true,
            self.tool_restore_report.take(),
        );
        let mut input = admissions
            .turn_inputs
            .first()
            .map(crate::AdmittedTurnInputs::materialize_turn_input)
            .unwrap_or_else(TurnInput::empty);
        input.trace_turn_id = Some(run.clone());
        let turn_boundary = self
            .host
            .core
            .tracing
            .record_boundary(
                controller,
                format!("trace:turn:{run}:started"),
                lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
                    session_id: self.state.session_id.clone(),
                    turn_id: run.clone(),
                }),
                controller.trace_scope().map_or(
                    lash_trace::TraceCause::Root,
                    lash_trace::DurableTraceScope::parent_cause,
                ),
                None,
                lash_trace::TraceTransitionKind::Started,
            )
            .await
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        let controller = controller.with_trace_scope(turn_boundary.scope.clone());
        let previous_prompt_usage = self.state.last_prompt_usage.clone();
        let turn_index = self.state.turn_index + 1;
        let state_segment = u32::try_from(turn_index).map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::Plugin,
                "physical turn index exceeds the plugin-state segment range",
            )
        })?;
        self.services
            .plugins
            .adopt_state_segment(crate::tool_run::SegmentOrdinal(state_segment));
        let (normalized, invalid_input) = match self.normalize_input_items(&input.items).await {
            Ok(items) => (items, None),
            Err(error) => (Vec::new(), Some(error)),
        };

        let mut turn_delta = Vec::new();
        let initial_turn_causes: Vec<_> = admissions
            .queued
            .iter()
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
            .unwrap_or_else(|| format!("m_turn_{run}_input"));
        let mut user_parts: Vec<Part> = Vec::new();
        let trace_metadata = prepare::turn_trace_metadata(&self.state, normalized.len());
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
        if invalid_input.is_none() {
            if user_parts.is_empty() && initial_turn_causes.is_empty() {
                user_parts.push(Part::text(format!("{user_id}.p0"), String::new(), None));
            }
            if !user_parts.is_empty() {
                reassign_part_ids(&user_id, &mut user_parts);
                turn_delta.push(Message {
                    id: user_id.clone(),
                    role: MessageRole::User,
                    parts: shared_parts(user_parts),
                    // Typed provenance, not a pinned id: a host that rendered
                    // its own row for this turn recognizes the committed copy
                    // by `turn_id` (FIG-972).
                    origin: Some(crate::MessageOrigin::TurnInput {
                        turn_id: run.clone(),
                        input_id: turn_input_id.clone(),
                    }),
                    reply_marker: None,
                });
            }
        }
        let mut initial_turn_input_applications = Vec::new();
        for admitted in &mut admissions.turn_inputs {
            admitted.record_initial_turn_application(run, &user_id);
            initial_turn_input_applications.extend(admitted.applications.iter().cloned());
        }
        if !initial_turn_input_applications.is_empty() {
            turn_observation_cursor(&controller, run, "prepare").observe(
                &observer.for_turn(run),
                crate::engine::ObservedEvent::Activity {
                    correlation_id: None,
                    event: TurnEvent::QueuedInputAccepted {
                        applications: initial_turn_input_applications,
                    },
                },
            );
        }

        // Context pressure runs before the turn's graph-append draft exists:
        // a frame it opens commits on its own, and the turn is drafted over
        // the frame the turn then runs in (FIG-4110).
        let pressure = self
            .run_context_pressure(ContextPressureStep {
                run: std::marker::PhantomData,
                trace_turn_id: run,
                previous_prompt_usage: previous_prompt_usage.clone(),
                scoped_effect_controller: &controller,
            })
            .await?;
        let previous_prompt_usage = if pressure.opened_frame {
            None
        } else {
            previous_prompt_usage
        };
        // One graph-append draft per turn: prepare-turn hooks, the driver's
        // hooks and finalize-turn hooks all record into it, and `turn.commit`
        // commits it with the turn.
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
            .runtime_session_services_for_turn(&turn_graph_appends)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let plugins = Arc::clone(
            self.session
                .as_ref()
                .expect("lash runtime session must be available")
                .plugins(),
        );
        let base_read_model = self.state.read_model();
        let prepare_read_view = self.read_view();
        let turn_ctx = crate::TurnTransformContext {
            session_id: self.state.session_id.clone(),
            plugin_config: self.state.admitted_plugin_config(),
            state: prepare_read_view,
            prompt_usage: previous_prompt_usage.clone(),
            max_context_tokens: LashRuntime::max_context_tokens(self).ok(),
            traces: manager.trace_emitter(),
            scoped_effect_controller: controller.clone(),
            direct_completions: manager.direct_completion_client(
                controller.clone(),
                Some(turn_phase_id(run, "prepare-turn")),
            ),
        };
        let messages =
            crate::MessageSequence::from_base_and_delta(base_read_model.messages, turn_delta)
                .with_base_render_cache(base_read_model.prompt_render_cache);
        let prepared_context = plugins
            .prepare_turn_context(
                &turn_ctx,
                crate::session_model::context::PreparedContext {
                    messages: messages.clone(),
                    ..Default::default()
                },
                self.turn_phase_probe.clone(),
            )
            .await
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::ContextPrepareTurn))?;
        drop(turn_ctx);
        let mut prelude = Box::new(crate::runtime::effect::TurnPrelude {
            configuration: crate::EffectAddress::new(
                controller.execution_scope().clone(),
                format!("turn-config:{run}"),
            )
            .map_err(RuntimeEffectControllerError::from)
            .map_err(RuntimeEffectControllerError::into_runtime_error)?,
            pressure: pressure.decisions,
            history: messages.clone(),
            context: prepared_context.clone(),
            before_turn: None,
        });
        if let Some(session) = self.session.as_mut() {
            session
                .set_context_overlay(
                    plugins
                        .resolve_context_tool_bindings(&prepared_context.tool_providers)
                        .map_err(|error| {
                            error.into_turn_failure(RuntimeErrorCode::ContextPrepareTurn)
                        })?,
                )
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::SessionToolRegistry, err.to_string())
                })?;
        }
        self.state.last_prompt_usage = None;

        // The before-turn hooks: real inputs, appended to the prompt view
        // without entering the history the commit writes.
        let turn_policy = self.state.effective_policy().clone();
        let effective_protocol_turn_options = self.state.effective_protocol_turn_options();
        let history_len = messages.len();
        let mut prepared = self
            .prepare_turn_preamble(execute::TurnPreambleContext {
                run: std::marker::PhantomData,
                plugins: &plugins,
                scoped_effect_controller: &controller,
                manager: &manager,
                messages,
                turn_policy: &turn_policy,
                effective_protocol_turn_options: &effective_protocol_turn_options,
                turn_context: &input.turn_context,
                turn_scope_id: run,
            })
            .await?;
        turn_graph_appends
            .apply_session_contributions(&self.state.session_id, &plugins, &prepared.session)
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginPrepareTurn))?;
        if prepared.messages.len() > history_len {
            prelude
                .context
                .messages
                .make_mut()
                .extend(prepared.messages.iter().skip(history_len).cloned());
        }
        prelude.history = prepared.messages.clone();
        if plugins.has_before_turn_hooks() {
            prelude.before_turn = Some(
                crate::EffectAddress::new(
                    controller.execution_scope().clone(),
                    format!("plugin-callbacks:before-turn:{run}"),
                )
                .map_err(RuntimeEffectControllerError::from)
                .map_err(RuntimeEffectControllerError::into_runtime_error)?,
            );
        }
        let mut recorded_assembly = RecordedTurnAssembly::new();
        for event in &prepared.events {
            recorded_assembly.record(event);
        }
        emit_session_events(observer, std::mem::take(&mut prepared.events));
        self.state.last_prompt_usage = previous_prompt_usage;
        let mut turn_pipeline = TurnBoundary::from_state_with_graph_appends(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(run),
            self.host.core.durability.commit_budget,
            turn_graph_appends.clone(),
        )
        .with_fleet_format(self.fleet_format())
        .with_definition_engines(self.host.core.process_engines.clone())
        .with_metrics(self.host.core.tracing.metrics().clone())
        .with_trace_metadata(trace_metadata)
        .with_trace(
            self.host
                .core
                .tracing
                .shift(controller.trace_scope().cloned(), &controller),
        );
        turn_pipeline
            .prepared_checkpoint(
                turn_policy.clone(),
                turn_index,
                &prepared.messages,
                self.session.as_mut(),
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let resolved_turn_policy = self
            .host
            .resolve_session_policy(&self.state.session_id, turn_policy)
            .map_err(crate::runtime::turn_config::llm_profile_unconfigured)?;
        let session = self
            .session
            .take()
            .expect("lash runtime session must be available");
        let driver = Box::new(RuntimeTurnDriver {
            run: std::marker::PhantomData,
            tool_run_owner: None,
            session,
            policy: resolved_turn_policy,
            prelude,
            recorded_assembly,
            host: self.host.clone(),
            turn_id: run.clone(),
            scoped_effect_controller: controller.clone(),
            session_id: self.state.session_id.clone(),
            turn_index,
            turn_pipeline,
            latest_prompt_usage: None,
            llm_calls: Vec::new(),
            failure_evidence: Vec::new(),
            session_services: manager,
            protocol_turn_options: effective_protocol_turn_options,
            turn_context: input.turn_context,
            turn_causes: initial_turn_causes,
            pending_queued: admissions.queued,
            pending_turn_inputs: admissions.turn_inputs,
            pending_checkpoint_turn_inputs: None,
            withheld_terminal_work: Default::default(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            protocol_reply: Default::default(),
            opener_state: crate::session::OpenerState::default(),
            children_stop: CancellationToken::new(),
            turn_observations: turn_observation_cursor(&controller, run, "shift"),
            trace: self.host.core.tracing.turn_execution(&controller),
        });
        Ok(DurableTurn {
            driver,
            messages: prepared.messages,
            invalid_input,
        })
    }
}
