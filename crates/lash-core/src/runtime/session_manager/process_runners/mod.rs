use super::*;

#[cfg(test)]
mod context_tests;
mod control;
mod session;

impl RuntimeSessionServices {
    /// The catalog a process's steps resolve against: its own plugin
    /// session's tools.
    pub(in crate::runtime) fn process_step_catalog(
        &self,
    ) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        // The process's plugins run under the admission its runtime adopted.
        self.current.plugins.materialize()?;
        self.current.plugins.resolved_tool_catalog()
    }

    /// The tools `process`'s steps run (ADR 0132 §10): its own plugin
    /// session's catalog, the round tools that pin and run a catalog tool as
    /// a turn's round does, under `cx`, the process actor's claimed context,
    /// and the context its host steps run over. Both run inside `process`:
    /// what a tool reads off the process it runs in, and what the children it
    /// starts inherit (its lineage, originator, captured environment and wake
    /// target), are the process's committed facts, read once when the
    /// activation builds its tools.
    ///
    /// # Errors
    ///
    /// The surface does not resolve, or the process's wake target cannot be
    /// read.
    pub(in crate::runtime) async fn process_step_tools(
        &self,
        cx: crate::ActorContext,
        process: &crate::ProcessRecord,
    ) -> Result<crate::runtime::ProcessStepTools, crate::PluginError> {
        use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
        let registry = self.current.host.process_registry().ok_or_else(|| {
            crate::PluginError::Session(
                "process registry is unavailable in this runtime".to_string(),
            )
        })?;
        let wake_session_id = registry.wake_target(&process.id).await?;
        self.current.plugins.materialize()?;
        let surface = self.current.plugins.pin_resolved_tool_surface()?;
        let catalog = Arc::clone(&surface.catalog);
        let dispatch = self.process_step_dispatch(surface, cx)?;
        let core = &self.current.host.core;
        let context = crate::RuntimeExecutionContext::new(
            dispatch,
            Arc::clone(&core.durability.process_env_store),
            Arc::clone(&core.durability.attachment_store),
            Arc::new(crate::ChronologicalProjection::default()),
            crate::TurnContext::default(),
            self.current.execution_env_spec()?,
        )
        .with_turn_phase_probe(self.current.turn_phase_probe.clone())
        .with_fleet_format(core.session_store_factory().fleet_format())
        .with_tool_material_store(core.backend().tool_material_store())
        .with_process_work(self.current.host.work.process_wiring().cloned())
        .with_process_execution(process, wake_session_id, None);
        let tools = context
            .round_tools(crate::EffectOpener::Process {
                process_id: process.id.clone(),
            })
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        Ok(crate::runtime::ProcessStepTools {
            catalog,
            tools,
            host: context,
        })
    }

    /// The dispatch a process's tool steps run on (ADR 0132 §10): `surface`,
    /// the process's own plugin session's tools, under `effect_controller`,
    /// the process actor's claimed context. The execution context that runs
    /// inside the process sets the lineage and originator its children start
    /// under (FIG-3607 R1).
    fn process_step_dispatch<'run>(
        &self,
        tool_surface: crate::plugin::ResolvedToolSurface,
        effect_controller: crate::ActorContext,
    ) -> Result<Arc<crate::tool_dispatch::ToolDispatchContext<'run>>, crate::PluginError> {
        let services = Arc::new(self.clone());
        let direct_completions = services.direct_completion_client(effect_controller.clone(), None);
        let execution_env_spec = self.current.execution_env_spec()?;
        let owner = self.current.execution_owner()?;
        Ok(Arc::new(crate::tool_dispatch::ToolDispatchContext {
            fleet_format: self
                .current
                .host
                .core
                .session_store_factory()
                .fleet_format(),
            plugins: Arc::clone(&self.current.plugins),
            tools: Arc::clone(&tool_surface.registry) as Arc<dyn crate::ToolProvider>,
            tool_registry: Some(Arc::clone(&tool_surface.registry)),
            tool_catalog: tool_surface.catalog,
            sessions: services.state_service(),
            session_lifecycle: services.lifecycle_service(),
            session_graph: services.graph_service(),
            processes: services.model_tool_process_service(),
            trigger_router: services.trigger_router(),
            process_engines: services.process_engines().clone(),
            effect_controller,
            direct_completions,
            parent_invocation: None,
            observation_call_key: None,
            execution_env_spec,
            owner,
            // A process's steps have no host lane: the dispatch observes
            // nowhere.
            observer: crate::engine::NullObservationSink::arc(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&self.current.host.core.durability.attachment_store),
            turn_context: crate::TurnContext::default(),
            clock: Arc::clone(&self.current.host.core.clock),
            process_lineage: None,
            process_originator: None,
        }))
    }
}
