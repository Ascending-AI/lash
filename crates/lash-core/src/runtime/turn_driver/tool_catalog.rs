use super::*;
use crate::sansio::ExecutionEnvironmentSyncFailureKind as SyncFailureKind;
pub(in crate::runtime) use crate::session::ExecutionEnvironmentSyncError as SyncFailure;
use crate::{PluginError, TurnDriverPreamble};

pub(super) struct PreparedExecutionEnvironment {
    pub(super) tool_definitions: Vec<crate::ToolDefinition>,
    turn_driver_preamble: Arc<TurnDriverPreamble>,
}

impl RuntimeTurnDriver<'_> {
    /// The turn's machine over `messages`, counting its protocol iterations
    /// on from `run_offset`.
    #[expect(
        clippy::expect_used,
        reason = "an admitted turn has a committed active agent frame and an opener scope"
    )]
    pub(super) fn prepare_machine(
        &mut self,
        messages: crate::MessageSequence,
        run_offset: usize,
    ) -> TurnMachine {
        let session_policy = self.policy.clone();
        // The machine starts with no environment: its protocol-start sync
        // builds the prompt and the tool surface as a recorded step, so the
        // shift never reads a surface a replay could not reproduce (FIG-3672
        // P7b). Only the protocol's driver configuration is taken here, and
        // that is host configuration, independent of the tools.
        self.mark_phase_begin(RuntimeTurnPhase::PromptBuild);
        let turn_driver_preamble = self.session.protocol_driver_preamble();
        // ADR 0117: the model's calls are named under the admitted scope's
        // run, continued by this physical turn.
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
            model: session_policy.llm_profile_config().clone(),
            messages,
            events: self.turn_pipeline.active_events(),
            protocol_run_offset: run_offset,
            turn_driver_preamble,
            turn_budget: session_policy.turn_budget,
            no_progress_budget: session_policy.no_progress_budget,
            attachment_acceptance: Arc::clone(&session_policy.attachment_acceptance),
            generation: session_policy.generation.clone(),
            emit_llm_trace: false,
            termination: self.protocol_turn_options.clone(),
            model_tool_calls,
        });
        self.mark_phase_end(RuntimeTurnPhase::PromptBuild);
        let mut machine = prepared.machine;
        machine.adopt_prepared_messages(self.prelude.context.messages.clone());
        machine
    }

    /// The step body of an execution-environment sync: builds the tool
    /// surface over the live registry and returns it with the surface's
    /// definitions, which the sync records. It installs nothing: the shift
    /// installs the surface the sync recorded. It composes no prompt: each
    /// model call composes its own at admission (ADR 0133 §6), over the
    /// surface installed here.
    pub(in crate::runtime) fn refresh_execution_environment(
        &self,
    ) -> Result<
        (
            crate::sansio::ExecutionEnvironmentSync,
            Vec<crate::ToolDefinition>,
        ),
        SyncFailure,
    > {
        let execution_environment = self
            .prepare_execution_environment()
            .map_err(|error| SyncFailure::of_plugin_error(SyncFailureKind::ToolSurface, error))?;
        Ok((
            crate::sansio::ExecutionEnvironmentSync {
                tool_specs: execution_environment
                    .turn_driver_preamble
                    .tool_specs
                    .clone(),
                turn_controls: execution_environment
                    .tool_definitions
                    .iter()
                    .map(crate::ToolDefinition::manifest)
                    .filter(|manifest| !manifest.declaration().controls.is_empty())
                    .map(|manifest| {
                        let controls = manifest.declaration().controls.clone();
                        (manifest.name, controls)
                    })
                    .collect(),
            },
            execution_environment.tool_definitions,
        ))
    }

    pub(super) fn prepare_execution_environment(
        &self,
    ) -> Result<PreparedExecutionEnvironment, PluginError> {
        let state = self.turn_pipeline.state();
        let tool_surface = self
            .session
            .pin_tool_surface(&state.authority.tool_access)?;
        Ok(PreparedExecutionEnvironment {
            tool_definitions: tool_surface.definitions(),
            turn_driver_preamble: tool_surface.preamble(),
        })
    }

    pub(super) fn trace_before_llm_call(&mut self, machine: &TurnMachine, request: &LlmRequest) {
        if self.trace.is_observed() {
            let tool_fingerprints = self.session.composition_tool_fingerprints(&request.tools);
            let fingerprint =
                crate::trace::trace_composition_key(request, tool_fingerprints.as_slice());
            if self
                .session
                .record_composition_trace_fingerprint(fingerprint)
            {
                let snapshot = crate::trace::trace_composition_snapshot(
                    request,
                    fingerprint,
                    self.trace.content(),
                );
                self.emit_trace(machine.protocol_iteration(), || {
                    lash_trace::TraceEvent::CompositionChanged {
                        fingerprint: snapshot.fingerprint,
                        rendered_system_prompt: snapshot.rendered_system_prompt,
                        tool_schemas: snapshot.tool_schemas,
                    }
                });
            }
        }
    }

    pub(super) async fn run_before_llm_call(
        &mut self,
        messages: crate::MessageSequence,
        protocol_iteration: usize,
        request: &LlmRequest,
        last_call_usage: Option<crate::LlmUsage>,
    ) -> Result<Option<crate::ProtocolLlmCallAction>, PluginError> {
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
                    latest_prompt_usage: last_call_usage,
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
    pub(super) fn validate_recorded_selection(&self) -> Result<(), Box<SessionStreamEvent>> {
        let recorded = self.policy.llm_profile_config();
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
                    lash_sansio::session_model::RuntimeOutputCuts::standard(),
                )));
            }
        }
        Ok(())
    }
}
