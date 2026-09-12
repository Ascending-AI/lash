//! The prepare phase: refresh the resident graph, materialize the claimed
//! input, normalize its items, and run the context transform that produces the
//! message sequence the execute phase drives.

use super::*;

/// Everything the prepare phase needs to turn a claimed [`TurnInput`] into a
/// driven physical turn.
///
/// The accept phase builds one of these and the prepare phase consumes it; the
/// fields are the phase's inputs in the order the phase reads them.
pub(in crate::runtime) struct TurnPrepareContext<'sinks, 'run> {
    pub(in crate::runtime) input: TurnInput,
    pub(in crate::runtime) sinks: TurnSinks<'sinks>,
    pub(in crate::runtime) scoped_effect_controller: ScopedEffectController<'run>,
    pub(in crate::runtime) cancel: CancellationToken,
    pub(in crate::runtime) queued_claims: Vec<crate::QueuedWorkClaim>,
    pub(in crate::runtime) turn_input_claims: Vec<super::turn_input_ingress::TurnInputDrive>,
    pub(in crate::runtime) materialize_initial_claims: bool,
    pub(in crate::runtime) lease: TurnLeaseScope<'sinks>,
}

impl LashRuntime {
    pub(super) async fn stream_turn_inner(
        &mut self,
        context: TurnPrepareContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let TurnPrepareContext {
            mut input,
            sinks: TurnSinks {
                events,
                turn_events,
            },
            scoped_effect_controller,
            cancel,
            queued_claims,
            mut turn_input_claims,
            materialize_initial_claims,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
        } = context;
        self.reload_invalidated_resident_session_state_under_lease(session_execution_lease)
            .await?;
        let lease_continuity =
            session_execution_lease.and_then(SessionExecutionLeaseGuard::continuity);
        let resident_graph_is_current = self
            .resident_session
            .graph_is_current_under(lease_continuity);
        if !resident_graph_is_current {
            self.refresh_session_graph_from_store()
                .await
                .map_err(session_head_refresh_error)?;
        }
        // `load_session` refreshes the committed graph/head, checkpoint,
        // config, frames, and token ledger. It does not cover pending turn
        // inputs, queued work, or trigger deliveries; those remain external
        // ingress and are picked up by their fenced claim paths.
        let input_trace_turn_id = input.trace_turn_id.clone();
        let queued_turn_work = materialize_initial_claims
            .then(|| queued_claims.first())
            .flatten()
            .map(crate::QueuedWorkClaim::materialize_queued_turn_work);
        let pending_turn_input = materialize_initial_claims
            .then(|| turn_input_claims.first())
            .flatten()
            .map(super::turn_input_ingress::TurnInputDrive::materialize_turn_input);
        if let Some(work) = pending_turn_input.as_ref()
            && input.items.is_empty()
        {
            let turn_context = input.turn_context.clone();
            input = work.clone();
            // Retain host controls installed on the initially materialized input. The claim is
            // rematerialized here to refresh durable payloads, not to erase live run policy.
            input.turn_context = turn_context;
            if input.trace_turn_id.is_none() {
                input.trace_turn_id = input_trace_turn_id.clone();
            }
        }
        if let Some(work) = queued_turn_work.as_ref()
            && input.items.is_empty()
        {
            let turn_context = input.turn_context.clone();
            input = work.input.clone();
            input.turn_context = turn_context;
            if input.trace_turn_id.is_none() {
                input.trace_turn_id = input_trace_turn_id;
            }
        }
        if self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .is_some()
        {
            ensure_durable_effect_input(&input)?;
        }
        if let Some(extension) = &input.protocol_extension
            && let Some(session) = self.session.as_ref()
        {
            let protocol_session = std::sync::Arc::clone(session.plugins().protocol_session());
            protocol_session
                .validate_turn_extension(extension)
                .await
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorCode::ProtocolTurnExtension, err.to_string())
                })?;
        }
        let previous_prompt_usage = self.state.last_prompt_usage.clone();
        let normalized = match self.normalize_input_items(&input.items).await {
            Ok(items) => items,
            Err(e) => {
                self.state.last_prompt_usage = None;
                let mut assembler = TurnAssembler::default();
                let trace_turn_id = input
                    .trace_turn_id
                    .clone()
                    .expect("turn id is bound from the execution scope before validation");
                emit_terminal_sequence(
                    &mut assembler,
                    events,
                    Some(TerminalDiagnostic {
                        kind: TerminalDiagnosticKind::InputValidation,
                        code: Some("invalid_turn_input".to_string()),
                        message: e,
                        retryable: Some(false),
                        activity: TerminalActivityTarget::UnscopedSink {
                            sink: turn_events,
                            turn_id: &trace_turn_id,
                        },
                    }),
                    TurnStop::InvalidInput,
                )
                .await;
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                let turn_index = self.state.turn_index + 1;
                let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
                let turn_control_binding =
                    turn_control_binding(turn_control_host.as_ref(), &scoped_effect_controller)
                        .await?;
                let turn_control_resolver = match &turn_control_binding {
                    crate::TurnControlBinding::HostOwned { resolver, peek: _ }
                    | crate::TurnControlBinding::RunScoped {
                        resolver,
                        durable_cancel_after_llm: _,
                    } => *resolver,
                };
                let turn_control = ActiveTurnControl::new(
                    turn_control_resolver,
                    TurnAddress::new(&self.state.session_id, &trace_turn_id),
                )
                .await?
                .with_local_cancel_origin(input.turn_context.local_cancel_origin_hint());
                let messages = crate::MessageSequence::from_base(self.state.read_model().messages);
                let mut turn_pipeline = TurnBoundary::from_state_with_clock(
                    self.state.clone(),
                    Arc::clone(&self.host.core.clock),
                    self.state.turn_scope(&trace_turn_id),
                    self.host.core.durability.commit_budget,
                );
                turn_pipeline.apply_prepared_messages(&messages);
                let claims = LogicalTurnClaims::new(queued_claims, turn_input_claims);
                return Box::pin(self.finish_turn(TurnCommitContext {
                    finish: TurnFinishInput {
                        turn_pipeline,
                        assembler,
                        new_messages: messages,
                        policy: self.state.effective_policy().clone(),
                        turn_index,
                        trace_turn_id,
                    },
                    claims: &claims,
                    events,
                    scoped_effect_controller: &scoped_effect_controller,
                    cancel_state: &cancel,
                    lease: TurnLeaseScope {
                        guard: session_execution_lease,
                        release_policy: session_execution_lease_release_policy,
                    },
                    turn_control: &turn_control,
                }))
                .await;
            }
        };
        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
        let turn_index = self.state.turn_index + 1;
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

        let base_read_model = self.state.read_model();
        let base_messages = base_read_model.messages;
        let base_render_cache = base_read_model.prompt_render_cache;
        let mut turn_delta = Vec::new();
        let initial_turn_causes = queued_turn_work
            .as_ref()
            .map(|work| work.turn_causes.clone())
            .unwrap_or_default();
        turn_delta.extend(
            initial_turn_causes
                .iter()
                .map(crate::TurnCause::to_event_message),
        );

        let turn_input_id = turn_input_claims
            .iter()
            .flat_map(|drive| drive.inputs().iter().map(|input| input.input_id.clone()))
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
        for claim in &mut turn_input_claims {
            claim
                .record_initial_turn_application(&crate::TurnId::from(&trace_turn_id), &user_id)?;
            initial_turn_input_applications.extend(claim.applications().to_vec());
        }
        if !initial_turn_input_applications.is_empty() {
            emit_turn_activity_to_sink_for_turn(
                turn_events,
                &trace_turn_id,
                TurnActivity::independent(TurnEvent::QueuedInputAccepted {
                    applications: initial_turn_input_applications,
                }),
            )
            .await;
        }

        // One graph-append draft per physical turn: prepare-turn hooks, the
        // turn driver's hooks, and finalize-turn hooks all record into it and
        // the turn boundary commits it with the turn.
        let turn_graph_appends = TurnGraphAppendDraft::from_resident_state(
            &self.state,
            Arc::clone(&self.host.core.clock),
        );
        let manager = self
            .runtime_session_services_for_turn(None, session_execution_lease, &turn_graph_appends)
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let plugin_session = self
            .session
            .as_ref()
            .map(|s| Arc::clone(s.plugins()))
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::ContextPrepareTurn,
                    "runtime session not available",
                )
            })?;
        let prepare_phase_turn_id = turn_phase_id(&trace_turn_id, "prepare-turn");
        let prepare_phase_controller = scoped_effect_controller.clone();
        let turn_ctx = crate::TurnTransformContext {
            session_id: self.state.session_id.clone(),
            state: self.read_view(),
            prompt_usage: previous_prompt_usage.clone(),
            max_context_tokens: Some(LashRuntime::max_context_tokens(self)),
            sessions: manager.state_service(),
            session_lifecycle: manager.lifecycle_service(),
            session_graph: manager.graph_service(),
            scoped_effect_controller: scoped_effect_controller.clone(),
            direct_completions: manager.direct_completion_client(
                RuntimeEffectControllerHandle::borrowed(prepare_phase_controller),
                Some(prepare_phase_turn_id),
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
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, err.to_string())
            })?;
        self.mark_phase_end(RuntimeTurnPhase::ContextTransform);
        // Release the read-view's graph clone before the rest of the turn
        // runs. Keeping it alive into `stream_prepared_turn` forces the
        // post-turn `append_active_read_delta` to deep-clone the session
        // graph (Arc::make_mut with refcount > 1).
        drop(turn_ctx);
        let messages = prepared_context.messages;
        if let Some(session) = self.session.as_mut() {
            session
                .set_context_overlay(
                    prepared_context.tool_providers,
                    prepared_context.prompt_contributions,
                    prepared_context.include_base_tools,
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
                    protocol_turn_options: input.protocol_turn_options.clone(),
                    protocol_extension: input.protocol_extension.clone(),
                    turn_context: input.turn_context.clone(),
                    initial_turn_causes,
                    trace_turn_id,
                    turn_index,
                },
                sinks: TurnSinks {
                    events,
                    turn_events,
                },
                scoped_effect_controller,
                cancel,
                initial_queue_claims: queued_claims,
                initial_turn_input_claims: turn_input_claims,
                lease: TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
            },
            turn_graph_appends,
        ))
        .await
    }

    async fn normalize_input_items(
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
