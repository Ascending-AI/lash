use super::*;
pub(in crate::runtime) use crate::session::ExecutionEnvironmentSyncError as SyncFailure;
use lash_execution_contracts::plugin_error::PluginError;

/// What the one machine builder reads besides a turn's build rows: the
/// session the turn runs in, its resolved policy and protocol options, and
/// the events of the turn's draft over the window that draft started from.
/// None of it needs a claim, a store write or a plugin callback.
pub(in crate::runtime) struct MachineBasis<'a> {
    pub(in crate::runtime) session: &'a Session,
    pub(in crate::runtime) session_id: &'a SessionId,
    pub(in crate::runtime) turn_id: &'a lash_vocabulary::identity::TurnId,
    pub(in crate::runtime) policy: &'a RuntimeSessionPolicy,
    pub(in crate::runtime) protocol_turn_options: crate::ProtocolTurnOptions,
    /// The window the turn's draft started from, the session's head.
    pub(in crate::runtime) head_window: crate::runtime::durable::head::TurnWindow,
    /// The turn draft's events, led by `head_window`'s.
    pub(in crate::runtime) events: lash_protocol_types::AppendVec<SessionHistoryRecord>,
}

/// The turn's machine as its start built it, from its journaled `build`
/// inputs over `basis`, rebuilt over `pinned`, the window at the turn's
/// pin, when the head has moved past it since: a fresh turn, a resume and
/// the open-turn inspection all build the machine here.
///
/// # Errors
///
/// [`lash_sansio::ReplayDivergence`] when the machine cannot be rebuilt
/// over the pinned window.
pub(in crate::runtime) fn build_turn_machine(
    basis: MachineBasis<'_>,
    build: &crate::runtime::durable::turn_build::TurnBuild,
    pinned: Option<crate::runtime::durable::head::TurnWindow>,
) -> Result<TurnMachine, lash_sansio::ReplayDivergence> {
    #[cfg(feature = "perf-witness")]
    let _history_scope = lash_core_ids::perf_witness::history::Scope::enter(
        lash_core_ids::perf_witness::history::Component::RequestBuild,
    );
    let MachineBasis {
        session,
        session_id,
        turn_id,
        policy,
        protocol_turn_options,
        head_window,
        events,
    } = basis;
    // The machine starts with no environment: its protocol-start sync
    // builds the prompt and the tool surface as a recorded step, so the
    // shift never reads a surface a replay could not reproduce (FIG-3672
    // P7b). Only the protocol's driver configuration is taken here, and
    // that is host configuration, independent of the tools.
    let turn_driver_preamble = session.protocol_driver_preamble();
    // ADR 0117: the model's calls are named under the admitted scope's
    // run, continued by this physical turn.
    let model_tool_calls = lash_sansio::ModelToolCalls::new(
        crate::EffectOpener::turn(session_id.clone(), turn_id.clone()).tool_call_admission(),
        build.turn_index as u64,
    );
    let messages = head_window.then(build.messages.clone());
    let prepared = crate::build_turn(crate::SansIoTurnInput {
        session_id: session_id.clone(),
        agent_frame_id: build.frame.clone(),
        turn_id: turn_id.clone(),
        model: policy.llm_profile_config().clone(),
        window: head_window.clone(),
        messages,
        events,
        protocol_run_offset: 0,
        turn_driver_preamble,
        turn_budget: policy.turn_budget,
        no_progress_budget: policy.no_progress_budget,
        attachment_acceptance: Arc::clone(&policy.attachment_acceptance),
        generation: policy.generation.clone(),
        emit_llm_trace: false,
        termination: protocol_turn_options,
        model_tool_calls,
    });
    let mut machine = prepared.machine;
    let window = match pinned {
        Some(pinned) if pinned.pin() != head_window.pin() => {
            machine.rewindow(pinned.clone())?;
            pinned
        }
        _ => head_window,
    };
    // The attachment-omission decisions narrow the request's view only:
    // the machine's messages stay the turn's history (ADR 0133).
    let mut request_messages = window.then(build.messages.clone());
    let omissions = build.omissions();
    if !omissions.is_empty() {
        crate::plugin::apply_attachment_omissions(request_messages.make_mut(), &omissions);
    }
    machine.adopt_prepared_messages(request_messages);
    if let Err(event) = validate_recorded_selection(policy) {
        machine.fail_turn(*event);
    }
    Ok(machine)
}

impl RuntimeTurnDriver<'_> {
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
        let result = environment_sync(&adoption.handle);
        self.adopted_environment = Some(adoption);
        Ok(result)
    }

    /// The sync the turn answered in each protocol iteration its recorded
    /// configuration holds a round for: each round's recorded environment,
    /// projected once, as [`Self::refresh_execution_environment`] answered
    /// it. A resumed turn's journal names its syncs by iteration.
    pub(in crate::runtime) async fn recorded_syncs(
        &self,
    ) -> Result<
        std::collections::BTreeMap<usize, crate::sansio::ExecutionEnvironmentSync>,
        crate::runtime::durable::session::TurnError,
    > {
        let store = self.host.core.backend().durable().environments();
        let mut projected = std::collections::BTreeMap::new();
        let mut syncs = std::collections::BTreeMap::new();
        for (iteration, round) in self.run_config.environments() {
            let sync = match projected.get(&round.version) {
                Some(sync) => Clone::clone(sync),
                None => {
                    let body =
                        super::super::session_environment::load_body(store.as_ref(), round).await?;
                    let handle = self.session.project_environment(&body).map_err(|error| {
                        crate::runtime::durable::session::TurnError::Runtime(
                            crate::RuntimeEffectControllerError::from(error).into_runtime_error(),
                        )
                    })?;
                    let sync = environment_sync(&handle);
                    projected.insert(round.version.clone(), sync.clone());
                    sync
                }
            };
            syncs.insert(usize::try_from(iteration).unwrap_or(usize::MAX), sync);
        }
        Ok(syncs)
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
}

impl TurnEffectContext {
    pub(super) async fn run_before_llm_call(
        &mut self,
        messages: crate::MessageSequence,
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
                    state: self.checkpoint_state_view(messages),
                    latest_prompt_usage: last_call_usage,
                    run_spec: self.run_config.run().spec().clone(),
                },
                request,
            )
            .await
    }
}

/// The sync a turn's machine is answered with when `handle` is the
/// environment adopted for its protocol iteration.
fn environment_sync(
    handle: &crate::session::ToolCatalogHandle,
) -> crate::sansio::ExecutionEnvironmentSync {
    let definitions = handle.definitions();
    crate::sansio::ExecutionEnvironmentSync {
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
    }
}

/// The recorded wire model, once the recorded reasoning is judged against
/// the recorded capability. It reads recorded facts alone and names the
/// model by its recorded key, so a replay judges the same way on a
/// deployment that no longer serves the key (FIG-4404).
fn validate_recorded_selection(
    policy: &RuntimeSessionPolicy,
) -> Result<(), Box<SessionStreamEvent>> {
    let recorded = policy.llm_profile_config();
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
