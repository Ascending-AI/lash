//! A turn the session actor runs (ADR 0132 §4; L3, FIG-5172): everything
//! the turn holds in memory, prepared from committed state alone. The
//! session's head, the rows its run took and the configuration its run
//! recorded in its checkpoint, or resolves once before its first phase
//! commits it (FIG-6021); nothing here commits, and the phase runner drives
//! what it returns.

use super::*;

/// A turn prepared from committed state: its driver, the messages its
/// machine starts from, and its before-turn callbacks' decisions.
pub(in crate::runtime) struct DurableTurn {
    pub(in crate::runtime) driver: Box<RuntimeTurnDriver<'static>>,
    /// The committed window the turn starts from: the head the runtime
    /// stands at once its context pressure ran, pinned by that head, which
    /// the turn's checkpoints name instead of holding (FIG-5206, FIG-5955).
    pub(in crate::runtime) window: crate::runtime::durable::head::TurnWindow,
    /// The turn's messages, led by the window's.
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

/// What a durable turn's preparation starts from.
pub(in crate::runtime) enum TurnStart {
    /// A fresh turn, with the trace scope its admission retained.
    Fresh(Option<lash_trace::DurableTraceScope>),
    /// A resumed turn, with what its checkpoint recorded.
    Resumed(crate::runtime::durable::session::RecordedPreparation),
}

impl LashRuntime {
    /// Prepare run `run` of this session for the session actor: install the
    /// run's configuration, materialize its admitted input over the head,
    /// apply the before-turn decisions and the attachment-omission policies,
    /// and build the driver that answers the turn's effects under
    /// `controller`, the turn's own claimed context. `admissions` are the
    /// rows the run took. A fresh turn ([`TurnStart::Fresh`]) resolves its
    /// configuration after its plugin transition and before its session is
    /// activated, and runs its before-turn callbacks under the trace scope
    /// its `turn.admit` retained, whose export it owes; its first phase
    /// commit records all of it. A resumed one ([`TurnStart::Resumed`])
    /// passes what its checkpoint recorded: no configuration is resolved and
    /// no callback runs, and it reads the retained trace scope back
    /// (FIG-5363, FIG-5395, FIG-6021). No preparation proposes an admission. `held` is the head
    /// the session's owner holds, if it holds one: the turn's prelude takes
    /// its base from it when it is the window the prelude names
    /// (FIG-6634).
    #[expect(
        clippy::expect_used,
        reason = "the runtime session is installed for the whole preparation"
    )]
    pub(in crate::runtime) async fn prepare_durable_turn(
        &mut self,
        controller: &ActorContext,
        run: &TurnId,
        mut admissions: LogicalTurnAdmissions,
        start: TurnStart,
        held: Option<&crate::runtime::durable::head::SessionHead>,
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
        self.transition_turn_plugins(controller).await?;
        let (recorded_before_turn, retained_trace, run_config, cell_start) = match start {
            TurnStart::Resumed(recorded) => (
                Some(recorded.before_turn),
                recorded.trace,
                recorded.config,
                recorded.cell,
            ),
            TurnStart::Fresh(admitted_trace) => (
                None,
                admitted_trace,
                crate::runtime::durable::run_config::RecordedRunConfig::resolved(
                    self.resolve_run(admitted_run_spec.as_ref()).await?,
                ),
                None,
            ),
        };
        self.activate_turn_session().await?;
        self.apply_turn_config(run_config.run())?;
        Self::emit_physical_turn_start(observer, controller, run);
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
        let mut user_messages = Vec::new();
        for pending in admissions
            .turn_inputs
            .iter()
            .flat_map(|admitted| &admitted.inputs)
        {
            let input = pending
                .input
                .live("PendingTurnInput")
                .map_err(|error| error.runtime_error())?;
            let normalized = normalize_input_items(&input.items);
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
        let prelude_history = crate::store::SessionHeadRef {
            generation: 0,
            revision: self.state.head_revision,
            leaf: self.state.session_graph.leaf_node_id.clone(),
            checkpoint: self.state.checkpoint_ref.clone(),
        };
        let pin = crate::runtime::durable::head::window_pin(&self.state).map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("the turn's window pin does not encode: {error}"),
            )
        })?;
        let window = crate::runtime::durable::head::TurnWindow::new(
            pin,
            base_read_model.messages,
            base_read_model.active_events,
        )
        .with_render_cache(base_read_model.prompt_render_cache);
        let messages = window.then(turn_delta);
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
        if !omissions.is_empty() {
            crate::plugin::apply_attachment_omissions(request_messages.make_mut(), &omissions);
        }
        let prepared_context = crate::session_model::context::PreparedContext {
            messages: request_messages,
        };
        drop(context_transform);
        let mut prelude = Box::new(crate::runtime::effect::TurnPrelude::new(
            crate::EffectAddress::new(
                controller.execution_scope().clone(),
                format!("turn-config:{run}"),
            )
            .map_err(RuntimeEffectControllerError::from)
            .map_err(RuntimeEffectControllerError::into_runtime_error)?,
            pressure.decisions,
            prelude_history,
            None,
        ));
        let store = self.services.store.as_ref().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ArtifactMissing,
                "turn preparation has no history store",
            )
        })?;
        // The runtime's read model may include a phase checkpoint's tail.
        // The base is the immutable graph window the prelude names: the
        // owner's held head when it is that window, else read from the
        // store; the tail belongs to the recorded view edits and needs no
        // separate checkpoint root.
        let held = held
            .filter(|head| head.state().session_id == self.state.session_id)
            .map(crate::runtime::durable::head::SessionHead::prelude_base);
        let (prelude_base_messages, _, _) = crate::runtime::durable::head::restore_prelude(
            &prelude,
            held.as_ref().map(|(head, base)| (head, base)),
            store,
        )
        .await
        .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        prelude.capture_messages(
            &prelude_base_messages,
            &messages,
            &prepared_context.messages,
        );
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
            .apply_session_contributions(&self.state.session_id, &prepared.session)
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
        // A durable turn continues from its committed checkpoint and is
        // never replayed: what its driver observes is this owner's own work,
        // so it observes as its own live attempt and needs no journal
        // frontier (ADR 0132 §2). Behind a frontier no step body crosses,
        // its protocol steps and compositions were dropped (FIG-5331).
        let turn_trace = self
            .host
            .core
            .tracing
            .unreplayed(controller.trace_scope().cloned());
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
        .with_trace(turn_trace.clone());
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
            prelude_base_messages,
            prepared_context,
            answered_cell_calls: Default::default(),
            cell_end: Default::default(),
            answered_cell_end: None,
            held_response: None,
            cell_start,
            answered_cell_close: None,
            session_pages: committed_session_pages(&controller, &self.state),
            host: self.host.clone(),
            turn_id: run.clone(),
            scoped_effect_controller: controller.clone(),
            session_id: self.state.session_id.clone(),
            turn_index,
            turn_pipeline,
            pending_model_calls: Vec::new(),
            staged_model_calls: Vec::new(),
            model_call_ordinal: 0,
            sealed_attempt_count: 0,
            provider_panic: None,
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
            model_stop_error: Default::default(),
            model_limit: None,
            call_drains: Default::default(),
            // The shift lane sequences what this drive does from where it
            // starts, so it is keyed by the drive: a successor, or a later
            // drive of this owner, never reuses an id an earlier drive
            // published (FIG-6274).
            turn_observations: turn_observation_cursor(
                &controller,
                run,
                &format!("shift@{}", controller.next_drive()),
            ),
            trace: turn_trace,
            admitted_body: None,
            model_attempt: 1,
            model_call_address: None,
            run_config,
            surface_installed: false,
        });
        Ok(DurableTurn {
            driver,
            window,
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

/// Where `state`'s committed head holds the session's execution state: the
/// head revision's row, at its checkpoint. None for a session that
/// committed no checkpoint yet, or a store that publishes no heap pages.
fn committed_session_pages(
    cx: &ActorContext,
    state: &crate::RuntimeSessionState,
) -> Option<crate::plugin::SessionPages> {
    crate::plugin::SessionPageReader {
        store: cx.pages().ok()?,
        actor: cx.actor().clone(),
        epoch: cx.epoch(),
    }
    .session_pages(state)
}
