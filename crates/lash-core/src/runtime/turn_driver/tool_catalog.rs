use super::*;
pub(in crate::runtime) use crate::session::ExecutionEnvironmentSyncError as SyncFailure;
use lash_execution_contracts::plugin_error::PluginError;

impl RuntimeTurnDriver<'_> {
    /// The turn's machine over `messages`, which lead with `window`'s,
    /// counting its protocol iterations on from `run_offset`.
    #[expect(
        clippy::expect_used,
        reason = "an admitted turn has a committed active agent frame and an opener scope"
    )]
    pub(super) fn prepare_machine(
        &mut self,
        window: crate::runtime::durable::head::TurnWindow,
        messages: crate::MessageSequence,
        run_offset: usize,
    ) -> TurnMachine {
        #[cfg(feature = "perf-witness")]
        let _history_scope = lash_core_ids::perf_witness::history::Scope::enter(
            lash_core_ids::perf_witness::history::Component::RequestBuild,
        );
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
            window,
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
        machine.adopt_prepared_messages(self.prepared_context.messages.clone());
        machine
    }

    /// Capture the facts this sync will adopt before deciding whether a held
    /// round outcome must commit ahead of an environment preparation.
    pub(super) fn prepare_environment_sync(&mut self) -> bool {
        let frontier = self
            .environment
            .capture_frontier(self.scoped_effect_controller.epoch());
        let may_prepare = frontier.may_prepare(
            self.adopted_environment.as_ref(),
            self.run_config.run().has_override(),
        );
        self.environment_frontier = Some(frontier);
        may_prepare
    }

    /// Adopt only the captured frontier, before this round's model call.
    pub(in crate::runtime) async fn refresh_execution_environment(
        &mut self,
    ) -> Result<
        crate::sansio::ExecutionEnvironmentSync,
        super::super::session_environment::EnvironmentAdmissionError,
    > {
        let frontier = self.environment_frontier.take().unwrap_or_else(|| {
            self.environment
                .capture_frontier(self.scoped_effect_controller.epoch())
        });
        let adoption = self
            .environment
            .adopt(
                &self.scoped_effect_controller,
                &self.session,
                self.run_config.run(),
                self.adopted_environment.as_ref(),
                frontier,
            )
            .await?;
        self.session.install_environment(adoption.handle.clone());
        let handle = &adoption.handle;
        let definitions = handle.definitions();
        let result = crate::sansio::ExecutionEnvironmentSync {
            tool_specs: handle.preamble().tool_specs.clone(),
            turn_controls: definitions
                .iter()
                .map(lash_vocabulary::tool_contract::ToolDefinition::manifest)
                .filter(|manifest| !manifest.declaration().controls.is_empty())
                .map(|manifest| (manifest.tool(), manifest.declaration().controls.clone()))
                .collect(),
            truncated_names: definitions
                .iter()
                .map(lash_vocabulary::tool_contract::ToolDefinition::manifest)
                .map(|manifest| (manifest.standard_name(), manifest.tool()))
                .filter(|(name, operation)| name.replace("__", ".") != *operation)
                .collect(),
        };
        self.adopted_environment = Some(adoption);
        Ok(result)
    }

    pub(super) fn trace_before_llm_call(&mut self, machine: &TurnMachine, request: &LlmRequest) {
        if self.trace.is_observed() {
            let runtime = self.trace.runtime();
            let tool_fingerprints = runtime.composition_tool_fingerprints(&request.tools);
            let fingerprint =
                crate::trace::trace_composition_key(request, tool_fingerprints.as_slice());
            if runtime.report_composition(&self.session_id, fingerprint) {
                let context = self.trace_context(machine.protocol_iteration());
                let trace = self
                    .trace
                    .selected(lash_trace::TraceSpanKind::Agent, &context.ids());
                let snapshot =
                    crate::trace::trace_composition_snapshot(request, fingerprint, trace.content());
                trace.observe(|| {
                    (
                        context,
                        lash_trace::TraceEvent::CompositionChanged {
                            fingerprint: snapshot.fingerprint,
                            rendered_system_prompt: snapshot.rendered_system_prompt,
                            tool_schemas: snapshot.tool_schemas,
                        },
                    )
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
        let bindings = self.session_services.bindings();
        self.session
            .plugins()
            .bind_services(Arc::clone(&bindings))?;
        self.session
            .plugins()
            .protocol_session()
            .before_llm_call(
                crate::ProtocolBeforeLlmCallContext {
                    session_id: self.session_id.clone(),
                    bindings,
                    state: self.checkpoint_state_view(messages, protocol_iteration),
                    latest_prompt_usage: last_call_usage,
                    run_spec: self.run_config.run().spec().clone(),
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
                    lash_protocol_types::session_model::RuntimeOutputCuts::standard(),
                )));
            }
        }
        Ok(())
    }
}
