use super::*;
use crate::sansio::ExecutionEnvironmentSyncFailureKind as SyncFailureKind;
pub(in crate::runtime) use crate::session::ExecutionEnvironmentSyncError as SyncFailure;
use crate::{PluginError, ToolCatalog, TurnDriverPreamble};

pub(super) struct PreparedExecutionEnvironment {
    tool_catalog: Arc<ToolCatalog>,
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
            autonomous: session_policy.autonomous,
            model: session_policy.llm_profile_config().clone(),
            messages,
            events: self.turn_pipeline.active_events(),
            turn_causes: self.turn_causes.clone(),
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
        machine.adopt_prepared_messages(self.prelude.context.messages.clone(), true);
        machine
    }

    /// The step body of an execution-environment sync: composes the prompt
    /// sections and builds the tool surface over the live registry, and
    /// returns both with the surface's definitions, which the sync records.
    /// It installs nothing: the shift installs the surface the sync recorded.
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
            .map_err(|error| SyncFailure::of_plugin_error(SyncFailureKind::ToolSurface, error))?;
        let composed = self
            .compose_turn_prompt(protocol_iteration, &execution_environment)
            .await?;
        self.trace_prompt_built(protocol_iteration, &composed);
        Ok((
            crate::sansio::ExecutionEnvironmentSync {
                instructions: composed.initial_instructions.map(Arc::from),
                current_context: composed.current_context.map(Arc::from),
                tool_specs: execution_environment
                    .turn_driver_preamble
                    .tool_specs
                    .clone(),
            },
            execution_environment.tool_definitions,
        ))
    }

    /// Compose the iteration's prompt sections (ADR 0133) under the running
    /// run's admitted plan and config (FIG-4589): a config command applied
    /// while this run executes reaches the next run, and a sync redriven
    /// after a redeploy serves what the run recorded. The cut offers the
    /// pinned tool surface, the session's committed usage and namespaces,
    /// and the protocol's facts.
    async fn compose_turn_prompt(
        &mut self,
        protocol_iteration: usize,
        execution_environment: &PreparedExecutionEnvironment,
    ) -> Result<crate::plugin::prompt::ComposedPrompt, SyncFailure> {
        use crate::plugin::prompt::{
            OfferedTools, ProjectedHistoryStats, PromptCall, PromptCut, PromptCutParts,
            PromptModel, PromptPurpose, PromptRenderPool,
        };
        let protocol_facts = self.protocol_prompt_facts().await.map_err(|error| {
            SyncFailure::of_session_error(SyncFailureKind::ProtocolFacts, error)
        })?;
        let plugins = Arc::clone(self.session.plugins());
        let state = self.turn_pipeline.state();
        let native = execution_environment
            .turn_driver_preamble
            .tool_specs
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<Vec<_>>();
        let callable = execution_environment
            .tool_catalog
            .tool_names()
            .iter()
            .filter(|name| !native.contains(name))
            .cloned()
            .collect();
        let history = self.prelude.context.messages.clone();
        let profile = self.policy.llm_profile_config();
        let iteration = u32::try_from(protocol_iteration).unwrap_or(u32::MAX);
        let cut = PromptCut::new(PromptCutParts {
            call: PromptCall {
                session_id: self.session_id.clone(),
                frame: state.current_frame_node_id.clone(),
                run: None,
                turn: Some(self.turn_id.clone()),
                iteration,
                call: iteration,
                purpose: PromptPurpose::Turn,
            },
            config: plugins.admitted_plugin_config(),
            session: Some(self.checkpoint_state_view(history.clone(), protocol_iteration)),
            offered: OfferedTools {
                native,
                callable,
                catalog: Arc::clone(&execution_environment.tool_catalog),
            },
            model: PromptModel {
                profile: Some(profile.key().clone()),
                context_window_tokens: Some(profile.context_window_tokens() as u64),
                committed_usage: state.last_prompt_usage.clone(),
            },
            history: ProjectedHistoryStats {
                messages: u32::try_from(history.len()).unwrap_or(u32::MAX),
                estimated_tokens: 0,
            },
            namespaces: plugins.committed_namespaces(),
        })
        .with_subagent(state.authority.subagent.clone())
        .with_protocol_facts(protocol_facts);
        plugins
            .prompt_catalog()
            .compose(
                &state.authority.prompt_plan,
                &PromptPurpose::Turn,
                Arc::new(cut),
                PromptRenderPool::shared(),
            )
            .await
            .map_err(SyncFailure::of_prompt_error)
    }

    fn trace_prompt_built(
        &self,
        protocol_iteration: usize,
        composed: &crate::plugin::prompt::ComposedPrompt,
    ) {
        if !self.trace.is_observed() {
            return;
        }
        let system_prompt = composed.initial_instructions.as_deref().unwrap_or("");
        let prompt_hash = lash_trace::sha256_hex(system_prompt.as_bytes());
        let prompt_chars = system_prompt.chars().count();
        let components = composed
            .snapshot
            .sections
            .iter()
            .filter_map(|section| match &section.value {
                crate::prompt_sections::RecordedSectionText::Text { text } => {
                    Some(lash_trace::TracePromptComponent {
                        id: section.section.to_string(),
                        kind: match section.placement {
                            crate::prompt_sections::PromptPlacement::InitialInstructions => {
                                "initial_instructions".to_string()
                            }
                            crate::prompt_sections::PromptPlacement::CurrentContext => {
                                "current_context".to_string()
                            }
                            crate::prompt_sections::PromptPlacement::Excluded => {
                                "excluded".to_string()
                            }
                        },
                        hash: text.blob.0.clone(),
                        chars: composed
                            .texts
                            .get(&text.blob)
                            .map(|text| text.chars().count()),
                    })
                }
                crate::prompt_sections::RecordedSectionText::Omitted => None,
            })
            .collect::<Vec<_>>();
        self.trace.observe(|| {
            (
                self.trace_context(protocol_iteration),
                lash_trace::TraceEvent::PromptBuilt {
                    prompt_hash: prompt_hash.clone(),
                    prompt_chars,
                    components: components.clone(),
                },
            )
        });
    }

    /// The protocol's facts for its prompt sections, derived from its
    /// committed execution state under the run's recorded render. The
    /// composed text they feed is journaled with each execution-environment
    /// sync, so a redriven iteration replays it rather than asking again
    /// (FIG-3538).
    async fn protocol_prompt_facts(
        &mut self,
    ) -> Result<Option<crate::plugin::prompt::ProtocolPromptFacts>, crate::SessionError> {
        let protocol_session = std::sync::Arc::clone(self.session.plugins().protocol_session());
        let recorded_render = self
            .turn_pipeline
            .state()
            .authority
            .run_view()
            .and_then(|view| view.run.render.as_ref());
        let mut context = crate::plugin::ProtocolSessionContext::new(
            &self.session_id,
            self.session.fleet_format(),
        );
        if let Some(recorded_render) = recorded_render {
            context = context.with_recorded_render(recorded_render);
        }
        protocol_session.prompt_facts(context).await
    }

    pub(super) fn prepare_execution_environment(
        &self,
    ) -> Result<PreparedExecutionEnvironment, PluginError> {
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
        if self.trace.is_observed() {
            let tool_fingerprints = self.session.composition_tool_fingerprints(&request.tools);
            let fingerprint =
                crate::trace::trace_composition_key(request, tool_fingerprints.as_slice());
            if self
                .session
                .record_composition_trace_fingerprint(fingerprint)
            {
                let snapshot = crate::trace::trace_composition_snapshot(request, fingerprint);
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
                )));
            }
        }
        Ok(())
    }
}
