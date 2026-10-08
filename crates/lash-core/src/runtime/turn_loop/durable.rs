//! A turn the session actor runs (ADR 0132 §4; L3, FIG-5172): everything
//! the turn holds in memory, prepared from committed state alone. The
//! session's head, the rows its run took and the configuration its run
//! recorded; nothing here commits, and the phase runner drives what it
//! returns.

use super::*;

/// A turn prepared from committed state: its driver, the messages its
/// machine starts from, and its before-turn callbacks' decisions.
pub(in crate::runtime) struct DurableTurn {
    pub(in crate::runtime) driver: Box<RuntimeTurnDriver<'static>>,
    pub(in crate::runtime) messages: crate::MessageSequence,
    /// What the before-turn callbacks decided: run for a fresh turn, served
    /// from the checkpoint for a resumed one. Every phase commits them.
    pub(in crate::runtime) before_turn: Vec<crate::plugin::RecordedTurnContribution>,
    /// Why the admitted input did not normalize: the turn ends at once,
    /// `InvalidInput`, without calling the model.
    pub(in crate::runtime) invalid_input: Option<String>,
}

impl LashRuntime {
    /// Prepare run `run` of this session for the session actor: install the
    /// run's recorded configuration, materialize its admitted input over the
    /// head, apply the before-turn decisions and the attachment-omission
    /// policies, and build the driver that answers the turn's effects under
    /// `controller`, the turn's own claimed context. `admissions` are the
    /// rows the run took. A fresh turn runs its before-turn callbacks; a
    /// resumed one passes the decisions its checkpoint recorded as
    /// `recorded_before_turn`, and no callback runs.
    #[expect(
        clippy::expect_used,
        reason = "the runtime session is installed for the whole preparation"
    )]
    pub(in crate::runtime) async fn prepare_durable_turn(
        &mut self,
        controller: &ActorContext,
        run: &TurnId,
        mut admissions: LogicalTurnAdmissions,
        recorded_before_turn: Option<Vec<crate::plugin::RecordedTurnContribution>>,
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
        self.resolve_turn_config(controller, run, admitted_run_spec.as_ref())
            .await?;
        Self::emit_physical_turn_start(
            observer,
            controller,
            run,
            &admissions,
            self.tool_restore_report.take(),
        );
        let turn_context = crate::TurnContext::default();
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
        let mut invalid_input = None;
        let mut input_item_count = 0;
        let mut user_messages = Vec::new();
        for pending in admissions
            .turn_inputs
            .iter()
            .flat_map(|admitted| &admitted.inputs)
        {
            let normalized = match self.normalize_input_items(&pending.input.items).await {
                Ok(items) => items,
                Err(error) => {
                    invalid_input = Some(error);
                    break;
                }
            };
            input_item_count += normalized.len();
            let user_id = crate::runtime::ingress_message_id(&pending.input_id);
            user_messages.push(opening_user_message(
                user_id,
                run,
                Some(pending.input_id.clone()),
                normalized,
            ));
        }
        // A run without host input or wake causes still opens with an empty
        // user message. Invalid input contributes no user messages.
        if user_messages.is_empty() && initial_turn_causes.is_empty() && invalid_input.is_none() {
            user_messages.push(opening_user_message(
                format!("m_turn_{run}_input"),
                run,
                None,
                Vec::new(),
            ));
        }
        if invalid_input.is_some() {
            user_messages.clear();
        }
        turn_delta.extend(user_messages);
        let trace_metadata = prepare::turn_trace_metadata(&self.state, input_item_count);
        let mut initial_turn_input_applications = Vec::new();
        for admitted in &mut admissions.turn_inputs {
            admitted.record_initial_turn_application(run, &turn_delta);
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
        let messages =
            crate::MessageSequence::from_base_and_delta(base_read_model.messages, turn_delta)
                .with_base_render_cache(base_read_model.prompt_render_cache);
        // The attachment-omission history policies (ADR 0133) narrow the
        // request's view only: `messages` stays the turn's history.
        let mut request_messages = messages.clone();
        let trace_context = {
            let context =
                lash_trace::TraceContext::default().for_session(self.state.session_id.clone());
            match controller.turn_id() {
                Some(turn_id) => context.for_turn(turn_id),
                None => context,
            }
        };
        let omissions = plugins
            .attachment_omissions(
                &crate::plugin::AttachmentOmissionContext {
                    session_id: self.state.session_id.clone(),
                    plugin_config: self.state.admitted_plugin_config(),
                    state: self.read_view(),
                    prompt_usage: previous_prompt_usage.clone(),
                    max_context_tokens: LashRuntime::max_context_tokens(self).ok(),
                    traces: manager.trace_emitter(),
                    trace_context,
                },
                &request_messages,
            )
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::ContextPrepareTurn))?;
        crate::plugin::apply_attachment_omissions(request_messages.make_mut(), &omissions);
        let prepared_context = crate::session_model::context::PreparedContext {
            messages: request_messages,
        };
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
        self.state.last_prompt_usage = None;

        // The before-turn hooks: their session changes and events.
        let turn_policy = self.state.effective_policy().clone();
        let effective_protocol_turn_options = self.state.effective_protocol_turn_options();
        let before_turn = match recorded_before_turn {
            Some(recorded) => recorded,
            None => {
                self.prepare_turn_preamble(execute::TurnPreambleContext {
                    run: std::marker::PhantomData,
                    plugins: &plugins,
                    scoped_effect_controller: &controller,
                    manager: &manager,
                    turn_policy: &turn_policy,
                    effective_protocol_turn_options: &effective_protocol_turn_options,
                    turn_context: &turn_context,
                    turn_scope_id: run,
                })
                .await?
            }
        };
        let mut prepared = crate::PluginSession::apply_before_turn(before_turn.clone());
        turn_graph_appends
            .apply_session_contributions(&self.state.session_id, &plugins, &prepared.session)
            .map_err(|err| err.into_turn_failure(RuntimeErrorCode::PluginPrepareTurn))?;
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
                &messages,
                self.session.as_mut(),
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let after_turn_reads = if plugins.has_after_turn_hooks() {
            Some(self.session_read_service().map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?)
        } else {
            None
        };
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
            after_turn_reads,
            protocol_turn_options: effective_protocol_turn_options,
            turn_context,
            turn_causes: initial_turn_causes,
            pending_queued: admissions.queued,
            pending_turn_inputs: admissions.turn_inputs,
            pending_checkpoint_turn_inputs: None,
            turn_phase_probe: self.turn_phase_probe.clone(),
            protocol_reply: Default::default(),
            opener_state: crate::session::OpenerState::default(),
            children_stop: CancellationToken::new(),
            turn_observations: turn_observation_cursor(&controller, run, "shift"),
            trace: self.host.core.tracing.turn_execution(&controller),
            admitted_body: None,
        });
        Ok(DurableTurn {
            driver,
            messages,
            before_turn,
            invalid_input,
        })
    }
}

/// One opening message per accepted input: normalization may combine text
/// within an input, but never across input provenance (ADR 0129, FIG-5288).
fn opening_user_message(
    user_id: String,
    run: &TurnId,
    input_id: Option<crate::InputId>,
    normalized: Vec<NormalizedItem>,
) -> Message {
    let mut parts = Vec::new();
    for item in normalized {
        let part_id = format!("{user_id}.p{}", parts.len());
        match item {
            NormalizedItem::Text(text) if !text.is_empty() => {
                parts.push(Part::text(part_id, text, None));
            }
            NormalizedItem::Text(_) => {}
            NormalizedItem::Attachment(source) => {
                parts.push(Part::attachment_part(
                    part_id,
                    String::new(),
                    Some(crate::session_model::message::PartAttachment { source }),
                ));
            }
        }
    }
    if parts.is_empty() {
        parts.push(Part::text(format!("{user_id}.p0"), String::new(), None));
    }
    Message {
        id: user_id,
        role: MessageRole::User,
        parts: shared_parts(parts),
        origin: Some(crate::MessageOrigin::TurnInput {
            turn_id: run.clone(),
            input_id,
        }),
        reply_marker: None,
    }
}
