use super::*;
use crate::{PluginError, ToolCatalog, TurnDriverPreamble};

struct PreparedExecutionEnvironment {
    tool_catalog: Arc<ToolCatalog>,
    tool_definitions: Vec<crate::ToolDefinition>,
    turn_driver_preamble: Arc<TurnDriverPreamble>,
}

impl RuntimeTurnDriver<'_> {
    #[expect(
        clippy::expect_used,
        reason = "an admitted turn has a committed active agent frame and an opener scope"
    )]
    pub(super) async fn prepare_turn_machine(
        &mut self,
        messages: crate::MessageSequence,
        event_tx: &TurnObserver,
        run_offset: usize,
    ) -> Result<TurnMachine, (crate::MessageSequence, usize)> {
        macro_rules! emit {
            ($event:expr) => {
                self.emit_recorded(event_tx, $event)
            };
        }

        let session_policy = self.policy.clone();
        let model = match self.validate_recorded_selection() {
            Ok(model) => model,
            Err(event) => {
                emit!(*event);
                emit!(SessionStreamEvent::Done);
                return Err((messages.clone(), run_offset));
            }
        };
        // The machine starts with no environment: its protocol-start sync
        // builds the prompt and the tool surface as a recorded step, so the
        // drive never reads a surface a replay could not reproduce (FIG-3672
        // P7b). Only the protocol's driver configuration is taken here, and
        // that is host configuration, independent of the tools.
        self.mark_phase_begin(RuntimeTurnPhase::PromptBuild);
        let turn_driver_preamble = self.session.protocol_driver_preamble();
        // ADR 0117: the model's calls are named under the admitted scope's
        // root, continued by this physical turn.
        let model_tool_calls = lash_sansio::ModelToolCalls::new(
            crate::EffectOpener::for_scope(self.scoped_effect_controller.admitted_scope())
                .expect("a turn runs under an opener scope")
                .tool_call_admission(),
            self.turn_index as u64,
        );
        let prepared = crate::build_turn(crate::SansIoTurnInput {
            session_id: self.session_id.clone(),
            agent_frame_id: self
                .turn_pipeline
                .state()
                .current_frame_node_id
                .clone()
                .expect("an admitted turn has a committed active agent frame")
                .into_inner(),
            turn_id: self.turn_id.clone(),
            autonomous: session_policy.autonomous,
            model,
            max_context_tokens: Some(session_policy.model_config().context_window_tokens()),
            messages,
            events: self.turn_pipeline.active_events(),
            turn_causes: self.turn_causes.clone(),
            protocol_run_offset: run_offset,
            turn_driver_preamble,
            projector_turn_inputs: Default::default(),
            turn_budget: session_policy.turn_budget,
            no_progress_budget: session_policy.no_progress_budget,
            model_variant: session_policy.model_config().reasoning.clone(),
            model_capability: session_policy.model_config().metadata().capability.clone(),
            attachment_acceptance: Arc::clone(&session_policy.attachment_acceptance),
            extra_body: session_policy.model_config().metadata().extra_body.clone(),
            request_defaults: session_policy
                .model_config()
                .metadata()
                .request_defaults
                .clone(),
            generation: session_policy.generation.clone(),
            emit_llm_trace: false,
            termination: self.protocol_turn_options.clone(),
            model_tool_calls,
        });
        self.mark_phase_end(RuntimeTurnPhase::PromptBuild);
        Ok(prepared.machine)
    }

    /// The step body of an execution-environment sync: builds the prompt and
    /// the tool surface over the live registry, and returns both with the
    /// surface's definitions, which the sync records. It installs nothing: the
    /// drive installs the surface the sync recorded.
    pub(in crate::runtime) async fn refresh_execution_environment(
        &mut self,
        protocol_iteration: usize,
    ) -> Result<
        (
            crate::sansio::ExecutionEnvironmentSync,
            Vec<crate::ToolDefinition>,
        ),
        SyncFailure,
    > {
        let execution_environment = self
            .prepare_execution_environment()
            .map_err(SyncFailure::of_plugin_error)?;
        // The protocol plugin renders the prompt from the running root's
        // admitted config and the pinned tool surface (FIG-4589): a config
        // command applied while this root runs reaches the next root, and a
        // sync redriven after a redeploy renders what the root recorded.
        let plugin_config = self.session.plugins().admitted_plugin_config();
        let protocol_session = Arc::clone(self.session.plugins().protocol_session());
        let system_prompt = protocol_session
            .render_system_prompt(crate::plugin::SystemPromptContext {
                plugin_config: &plugin_config,
                tool_catalog: execution_environment.tool_catalog.as_ref(),
                subagent: self.turn_pipeline.state().authority.subagent.as_ref(),
                purpose: crate::plugin::SystemPromptPurpose::Turn,
            })
            .await
            .map_err(SyncFailure::of_session_error)?;
        self.trace_prompt_built(protocol_iteration, &system_prompt);
        let projector_turn_inputs = self
            .projector_turn_inputs()
            .await
            .map_err(SyncFailure::of_session_error)?;

        Ok((
            crate::sansio::ExecutionEnvironmentSync {
                system_prompt,
                tool_specs: execution_environment
                    .turn_driver_preamble
                    .tool_specs
                    .clone(),
                projector_turn_inputs: Some(projector_turn_inputs),
            },
            execution_environment.tool_definitions,
        ))
    }

    fn trace_prompt_built(&self, protocol_iteration: usize, system_prompt: &str) {
        if self.host.core.tracing.trace_sink.is_none() {
            return;
        }
        let prompt_hash = lash_trace::sha256_hex(system_prompt.as_bytes());
        let prompt_chars = system_prompt.chars().count();
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            self.trace_context(protocol_iteration),
            lash_trace::TraceEvent::PromptBuilt {
                prompt_hash: prompt_hash.clone(),
                prompt_chars,
                components: vec![lash_trace::TracePromptComponent {
                    id: "system_prompt".to_string(),
                    kind: "rendered_prompt".to_string(),
                    hash: prompt_hash,
                    chars: Some(prompt_chars),
                }],
            },
            self.host.core.clock.as_ref(),
        );
    }

    /// The projector inputs derived from recorded turn state.
    ///
    /// `prompt_usage` is the previous turn's committed usage, held on the
    /// recorded session state; the bound-variables view is rendered by the
    /// protocol's session plugin. The results are installed into the machine
    /// config and journaled with each execution-environment sync, so a
    /// redriven iteration replays them rather than re-deriving them from live
    /// plugin cells (FIG-3538).
    async fn projector_turn_inputs(
        &mut self,
    ) -> Result<crate::sansio::ProjectorTurnInputs, crate::SessionError> {
        let protocol_session = std::sync::Arc::clone(self.session.plugins().protocol_session());
        let recorded_render = self
            .turn_pipeline
            .state()
            .authority
            .resolved_render
            .as_ref();
        let mut context = crate::plugin::ProtocolSessionContext::new(
            &self.session_id,
            self.session.fleet_format(),
        );
        if let Some(recorded_render) = recorded_render {
            context = context.with_recorded_render(recorded_render);
        }
        let bound_variables_prompt = protocol_session.bound_variables_prompt(context).await?;
        Ok(crate::sansio::ProjectorTurnInputs {
            prompt_usage: self.turn_pipeline.state().last_prompt_usage.clone(),
            bound_variables_prompt,
        })
    }

    fn prepare_execution_environment(&self) -> Result<PreparedExecutionEnvironment, PluginError> {
        let state = self.turn_pipeline.state();
        let tool_surface = self.session.pin_tool_surface(
            &state.authority.tool_access,
            state.authority.subagent.as_ref(),
        )?;
        Ok(PreparedExecutionEnvironment {
            tool_catalog: tool_surface.tool_catalog(),
            tool_definitions: tool_surface.definitions(),
            turn_driver_preamble: tool_surface.preamble(),
        })
    }

    pub(super) fn trace_before_llm_call(&mut self, machine: &TurnMachine, request: &LlmRequest) {
        if self.host.core.tracing.trace_sink.is_some() {
            let tool_fingerprints = self.session.composition_tool_fingerprints(&request.tools);
            let fingerprint =
                crate::trace::trace_composition_key(request, tool_fingerprints.as_slice());
            if self
                .session
                .record_composition_trace_fingerprint(fingerprint)
            {
                let snapshot = crate::trace::trace_composition_snapshot(request, fingerprint);
                self.emit_trace(
                    machine.protocol_iteration(),
                    lash_trace::TraceEvent::CompositionChanged {
                        fingerprint: snapshot.fingerprint,
                        rendered_system_prompt: snapshot.rendered_system_prompt,
                        tool_schemas: snapshot.tool_schemas,
                    },
                );
            }
        }
    }

    pub(super) async fn run_before_llm_call(
        &mut self,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        request: &LlmRequest,
    ) -> Result<Option<crate::ProtocolLlmCallAction>, PluginError> {
        let latest_prompt_usage = self.latest_prompt_usage.clone();
        self.session
            .plugins()
            .protocol_session()
            .before_llm_call(
                crate::ProtocolBeforeLlmCallContext {
                    session_id: self.session_id.clone(),
                    sessions: self.session_services.state_service(),
                    session_graph: self.session_services.graph_service(),
                    processes: self.session_services.process_service(),
                    state: self.checkpoint_state_view(messages, protocol_iteration),
                    latest_prompt_usage,
                },
                request,
            )
            .await
    }

    pub(super) fn checkpoint_state_view(
        &self,
        messages: crate::MessageSequence,
        _protocol_iteration: usize,
    ) -> crate::SessionReadView {
        self.turn_pipeline.read_view(
            self.policy.policy.clone(),
            self.turn_index,
            self.protocol_turn_options.clone(),
            messages,
        )
    }

    /// The recorded wire model, once the recorded reasoning is judged
    /// against the recorded capability. It reads recorded facts alone and
    /// names the model by its recorded key, so a replay judges the same way
    /// on a deployment that no longer serves the key (FIG-4404).
    pub(super) fn validate_recorded_selection(&self) -> Result<String, Box<SessionStreamEvent>> {
        let recorded = self.policy.model_config();
        let model = recorded.model.wire_model().to_string();
        // Effort names match exactly, so the selection travels unchanged.
        match recorded.metadata().capability.validate_selection(
            &model,
            recorded.key().as_str(),
            &recorded.reasoning,
        ) {
            Ok(()) => {}
            Err(error) => {
                return Err(Box::new(make_error_event(
                    crate::TurnFailureKind::LlmProvider,
                    Some(error.category.failure_code().into()),
                    error.message.clone(),
                    Some(error.message),
                )));
            }
        }
        Ok(model)
    }
}

/// Why an execution-environment sync could not rebuild the environment.
pub(in crate::runtime) enum SyncFailure {
    /// Deterministic over the turn's inputs: journaled as the sync's outcome,
    /// which fails the turn and replays identically.
    Recorded(String),
    /// A fact about this attempt — a store, lease or session fault — that must
    /// not be journaled: the turn aborts and a redrive rebuilds the
    /// environment (the FIG-3575 live-fault class).
    Live(crate::RuntimeError),
}

impl SyncFailure {
    fn of_plugin_error(error: PluginError) -> Self {
        let message = format!("protocol error: {error}");
        let failure = error.into_turn_failure(crate::RuntimeErrorCode::ProtocolBeforeLlmCall);
        if failure.turn_failure_cause().aborts_invocation() {
            Self::Live(failure)
        } else {
            Self::Recorded(message)
        }
    }

    fn of_session_error(error: crate::SessionError) -> Self {
        match error {
            crate::SessionError::Plugin(error) => Self::of_plugin_error(error),
            error @ crate::SessionError::Store { .. } => Self::Live(crate::RuntimeError::new(
                crate::RuntimeErrorCode::StoreCommitFailed,
                error.to_string(),
            )),
            error => Self::Recorded(error.to_string()),
        }
    }
}
