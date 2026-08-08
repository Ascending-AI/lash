use lash_sansio::core_support::*;
use lash_sansio::{PreparedPrompt, PromptCache, PromptContributionSet, PromptLayer};

use super::*;
use crate::{PluginError, ToolCatalog, TurnDriverPreamble};

struct PreparedExecutionEnvironment {
    tool_catalog: Arc<ToolCatalog>,
    turn_driver_preamble: Arc<TurnDriverPreamble>,
    prompt: PromptLayer,
}

impl PreparedExecutionEnvironment {
    fn build_prompt(
        &self,
        core_prompt: &PromptLayer,
        session_prompt: &PromptLayer,
        turn_prompt: &PromptLayer,
        prompt_cache: Option<Arc<PromptCache>>,
    ) -> PreparedPrompt {
        let mut capability_prompt = PromptLayer::new();
        for contribution in self
            .turn_driver_preamble
            .prompt_contributions
            .iter()
            .cloned()
        {
            capability_prompt.add_contribution(contribution);
        }
        let resolved = crate::resolve_prompt_layers([
            &capability_prompt,
            &self.prompt,
            core_prompt,
            session_prompt,
            turn_prompt,
        ]);
        let prompt_contributions = self
            .tool_catalog
            .filter_prompt_contributions(resolved.contributions);
        let contributions = PromptContributionSet::new(prompt_contributions);
        lash_sansio::build_prompt_cached(
            crate::PromptBuildInput {
                template_fingerprint: crate::prompt_template_fingerprint(&resolved.template),
                template: resolved.template,
                execution_prompt_fingerprint: crate::prompt_text_fingerprint(
                    &self.turn_driver_preamble.execution_prompt,
                ),
                execution_prompt: Arc::clone(&self.turn_driver_preamble.execution_prompt),
                tool_names_fingerprint: self.turn_driver_preamble.tool_names_fingerprint,
                tool_names: Arc::clone(&self.turn_driver_preamble.tool_names),
                contributions,
            },
            prompt_cache.as_deref(),
        )
    }
}

impl RuntimeTurnDriver<'_> {
    pub(super) async fn prepare_turn_machine(
        &mut self,
        messages: crate::MessageSequence,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        run_offset: usize,
    ) -> Result<TurnMachine, (crate::MessageSequence, usize)> {
        macro_rules! emit {
            ($event:expr) => {
                send_session_event(event_tx, $event).await
            };
        }

        let mut session_policy = self.policy.clone();
        let model = match self.prepare_provider(&mut session_policy).await {
            Ok(model) => model,
            Err(event) => {
                emit!(event);
                emit!(SessionStreamEvent::Done);
                return Err((messages.clone(), run_offset));
            }
        };
        let execution_environment = match self
            .prepare_execution_environment(&session_policy, self.turn_index, messages.clone())
            .await
        {
            Ok(surface) => surface,
            Err(err) => {
                emit!(make_error_event(
                    "plugin_prompt",
                    None,
                    err.to_string(),
                    Some(err.to_string()),
                ));
                emit!(SessionStreamEvent::Done);
                return Err((messages, run_offset));
            }
        };
        self.mark_phase_begin(RuntimeTurnPhase::PromptBuild);
        let prepared_prompt = execution_environment.build_prompt(
            &self.host.core.prompt.prompt,
            &session_policy.prompt,
            self.turn_context.prompt_layer(),
            Some(self.session.prompt_cache()),
        );
        let prepared = crate::build_turn(crate::SansIoTurnInput {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.to_string(),
            autonomous: session_policy.autonomous,
            model,
            max_context_tokens: Some(session_policy.context_window_tokens()),
            messages,
            events: self.turn_pipeline.active_events(),
            turn_causes: self.turn_causes.clone(),
            protocol_run_offset: run_offset,
            turn_driver_preamble: execution_environment.turn_driver_preamble,
            prepared_prompt,
            turn_budget: session_policy.turn_budget,
            model_variant: session_policy.model.variant.clone(),
            model_capability: session_policy.model.capability.clone(),
            generation: session_policy.generation.clone(),
            emit_llm_trace: false,
            termination: self.protocol_turn_options.clone(),
        });
        if self.host.core.tracing.trace_sink.is_some() {
            let prompt_hash =
                lash_trace::sha256_hex(prepared.prepared_prompt.system_prompt.as_bytes());
            let prompt_chars = prepared.prepared_prompt.system_prompt.chars().count();
            crate::trace::emit_trace(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                self.trace_context(run_offset),
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
        self.policy = session_policy;
        self.mark_phase_end(RuntimeTurnPhase::PromptBuild);
        Ok(prepared.machine)
    }

    pub(in crate::runtime) async fn refresh_execution_environment(
        &mut self,
        messages: crate::MessageSequence,
        update_machine_config: bool,
    ) -> Result<Option<crate::sansio::ExecutionEnvironmentSync>, crate::SessionError> {
        if !update_machine_config {
            return Ok(None);
        }

        let policy = self.policy.policy.clone();
        let execution_environment = self
            .prepare_execution_environment(&policy, self.turn_index, messages)
            .await
            .map_err(|err| crate::SessionError::Protocol(err.to_string()))?;
        let prepared_prompt = execution_environment.build_prompt(
            &self.host.core.prompt.prompt,
            &policy.prompt,
            self.turn_context.prompt_layer(),
            Some(self.session.prompt_cache()),
        );

        Ok(Some(crate::sansio::ExecutionEnvironmentSync {
            system_prompt: prepared_prompt.system_prompt,
            tool_specs: execution_environment
                .turn_driver_preamble
                .tool_specs
                .clone(),
        }))
    }

    async fn prepare_execution_environment(
        &mut self,
        session_policy: &SessionPolicy,
        turn_index: usize,
        messages: crate::MessageSequence,
    ) -> Result<PreparedExecutionEnvironment, PluginError> {
        let tool_catalog = self.session.resolved_tool_catalog(&self.session_id)?;
        let turn_driver_preamble = self.session.turn_driver_preamble(&self.session_id)?;
        let plugin_prompt_contributions = self
            .session
            .plugins()
            .collect_prompt_contributions(PromptHookContext {
                session_id: self.session_id.clone(),
                sessions: self.session_services.state_service(),
                state: self.turn_pipeline.read_view(
                    session_policy.clone(),
                    turn_index,
                    self.protocol_turn_options.clone(),
                    messages,
                ),
                protocol_turn_options: self.protocol_turn_options.clone(),
                turn_context: self.turn_context.clone(),
            })
            .await?;
        let mut prompt = PromptLayer::new();
        for contribution in self.session.context_prompt_contributions().iter().cloned() {
            prompt.add_contribution(contribution);
        }
        for contribution in plugin_prompt_contributions {
            prompt.add_contribution(contribution);
        }
        if let Some(extension) = &self.protocol_extension {
            for contribution in extension.prompt_contributions() {
                prompt.add_contribution(contribution);
            }
        }
        Ok(PreparedExecutionEnvironment {
            tool_catalog,
            turn_driver_preamble,
            prompt,
        })
    }

    pub(super) async fn before_llm_call(
        &mut self,
        machine: &TurnMachine,
        request: &LlmRequest,
    ) -> Result<Option<crate::ProtocolLlmCallAction>, PluginError> {
        let latest_prompt_usage = self.turn_pipeline.state_mut().last_prompt_usage.clone();
        self.session
            .plugins()
            .protocol_session()
            .before_llm_call(
                crate::ProtocolBeforeLlmCallContext {
                    session_id: self.session_id.clone(),
                    sessions: self.session_services.state_service(),
                    session_graph: self.session_services.graph_service(),
                    processes: self.session_services.process_service(),
                    state: self.checkpoint_state_view(
                        machine.message_sequence(),
                        machine.protocol_iteration(),
                    ),
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

    pub(super) async fn prepare_provider(
        &mut self,
        policy: &mut RuntimeSessionPolicy,
    ) -> Result<String, SessionStreamEvent> {
        let model = policy.model.id.clone();
        let provider_kind = policy.provider().kind();
        // Validate the requested effort against the host-supplied capability
        // and normalize (alias-clamp) it back onto the spec so the outgoing
        // request carries the canonical effort the provider expects.
        match policy.model.capability.validate_selection(
            &model,
            provider_kind,
            &policy.model.variant,
        ) {
            Ok(resolved) => policy.model.variant = resolved,
            Err(error) => {
                return Err(make_error_event(
                    "llm_provider",
                    Some(error.category.code()),
                    error.message.clone(),
                    Some(error.message),
                ));
            }
        }
        Ok(model)
    }
}
