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
    /// The trace scope the turn's admission retained: read back from its
    /// admission record for a fresh turn, from the checkpoint for a resumed
    /// one. Every phase commits it.
    pub(in crate::runtime) trace_scope: lash_trace::DurableTraceScope,
    /// The turn's execution, bound to the session's attachment store while
    /// the turn runs: every put of the turn is held by it (ADR 0124 §4).
    pub(in crate::runtime) attachments: Option<crate::attachments::AttachmentExecutionBinding>,
}

impl LashRuntime {
    /// Prepare run `run` of this session for the session actor: install the
    /// run's recorded configuration, materialize its admitted input over the
    /// head, apply the before-turn decisions and the attachment-omission
    /// policies, and build the driver that answers the turn's effects under
    /// `controller`, the turn's own claimed context. `admissions` are the
    /// rows the run took. A fresh turn runs its before-turn callbacks under
    /// `admitted_trace`, the trace scope its `turn.admit` retained, whose
    /// export it owes; a resumed one passes what its checkpoint recorded as
    /// `recorded`: no callback runs, and it reads the retained trace scope
    /// back (FIG-5363, FIG-5395). No preparation proposes an admission.
    #[expect(
        clippy::expect_used,
        reason = "the runtime session is installed for the whole preparation"
    )]
    pub(in crate::runtime) async fn prepare_durable_turn(
        &mut self,
        controller: &ActorContext,
        run: &TurnId,
        mut admissions: LogicalTurnAdmissions,
        recorded: Option<crate::runtime::durable::session::RecordedPreparation>,
        admitted_trace: Option<lash_trace::DurableTraceScope>,
        observer: &TurnObserver,
    ) -> Result<DurableTurn, RuntimeError> {
        // An admission never mixes run specs, so the head input's spec is the
        // run's; a run of queued work runs the default spec.
        let admitted_run_spec = admissions
            .turn_inputs
            .first()
            .and_then(|admitted| admitted.inputs.first())
            .and_then(|input| input.run_spec.clone());
        let attachments = self.bind_turn_attachments(controller)?;
        self.materialize_turn_session(controller).await?;
        self.resolve_turn_config(controller, run, admitted_run_spec.as_ref())
            .await?;
        Self::emit_physical_turn_start(observer, controller, run, self.tool_restore_report.take());
        let (recorded_before_turn, retained_trace) = match recorded {
            Some(recorded) => (Some(recorded.before_turn), recorded.trace),
            None => (None, admitted_trace),
        };
        let tracing = &self.host.core.tracing;
        // An admission that traced nothing leaves the turn untraced.
        let retained_trace = retained_trace.unwrap_or_else(|| {
            lash_trace::TraceScopeOffer::default().into_scope(
                lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
                    session_id: self.state.session_id.clone(),
                    turn_id: run.clone(),
                }),
                tracing.clock().timestamp_ms(),
            )
        });
        let turn_context = crate::TurnContext::default();
        let turn_boundary = tracing
            .record_boundary(
                controller,
                format!("trace:turn:{run}:started"),
                retained_trace,
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
        let mut input_item_count = 0;
        let mut user_messages = Vec::new();
        for pending in admissions
            .turn_inputs
            .iter()
            .flat_map(|admitted| &admitted.inputs)
        {
            let normalized = normalize_input_items(&pending.input.items);
            input_item_count += normalized.len();
            let user_id = crate::runtime::ingress_message_id(&pending.input_id);
            user_messages.push(opening_user_message(
                user_id,
                run,
                Some(pending.input_id.clone()),
                normalized,
            ));
        }
        // A run without host input still opens with an empty user message.
        if user_messages.is_empty() {
            user_messages.push(opening_user_message(
                format!("m_turn_{run}_input"),
                run,
                None,
                Vec::new(),
            ));
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
        let context_transform = crate::runtime::RuntimeNamedPhase::begin(
            self.turn_phase_probe.clone(),
            "context_transform",
        );
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
        drop(context_transform);
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
        let recorded_assembly = RecordedTurnAssembly::new();
        emit_session_events(observer, std::mem::take(&mut prepared.events));
        self.state.last_prompt_usage = previous_prompt_usage;
        let mut turn_pipeline = TurnBoundary::from_state_with_graph_appends(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(run),
            self.host.core.durability.commit_budget,
            turn_graph_appends.clone(),
        )
        .with_output_cuts(self.host.core.control.output_cuts)
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
            llm_calls: Vec::new(),
            failure_evidence: Vec::new(),
            session_services: manager,
            after_turn_reads,
            protocol_turn_options: effective_protocol_turn_options,
            turn_context,
            pending_turn_inputs: admissions.turn_inputs,
            pending_checkpoint_turn_inputs: None,
            turn_phase_probe: self.turn_phase_probe.clone(),
            protocol_reply: Default::default(),
            opener_state: crate::session::OpenerState::default(),
            children_stop: CancellationToken::new(),
            turn_observations: turn_observation_cursor(&controller, run, "shift"),
            trace: self.host.core.tracing.turn_execution(&controller),
            admitted_body: None,
            model_attempt: 1,
        });
        Ok(DurableTurn {
            driver,
            messages,
            before_turn,
            trace_scope: turn_boundary.scope,
            attachments,
        })
    }

    /// Bind the session's attachment store to the turn's execution before
    /// ingress, tools or plugins can put bytes, so a put of the turn is held
    /// by `Execution(j)` and, unless the turn's commit names it, ends when
    /// the turn settles (ADR 0124 §4). A resumed turn binds the same
    /// execution, so a put made before a crash stays held across it. A store
    /// with no session holder has no execution to bind.
    fn bind_turn_attachments(
        &self,
        controller: &ActorContext,
    ) -> Result<Option<crate::attachments::AttachmentExecutionBinding>, RuntimeError> {
        let store = &self.host.core.durability.attachment_store;
        if !matches!(
            store.holder(),
            crate::attachments::AttachmentHolder::Runtime(crate::RuntimeOwner::Session(_))
        ) {
            return Ok(None);
        }
        store
            .bind_execution_scoped(controller.execution_scope().journal_identity()?)
            .map(Some)
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::RuntimeEffectAttachmentStore,
                    error.to_string(),
                )
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
            NormalizedItem::Attachment(reference) => {
                parts.push(Part::attachment_part(
                    part_id,
                    String::new(),
                    Some(crate::session_model::message::PartAttachment { reference }),
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
