use super::*;

#[cfg(test)]
mod context_tests;
mod control;
mod session;

impl RuntimeSessionServices {
    /// The catalog an engine process's steps resolve against: the tools its
    /// environment's components provide that its environment's tools admit
    /// (D7).
    pub(in crate::runtime) fn process_step_catalog(
        &self,
    ) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        let (process_id, engine) = self.current.engine_process()?;
        let core = &self.current.host.core;
        engine
            .composition
            .tool_catalog(
                crate::RuntimeOwner::Process(process_id.clone()),
                engine.env.tools(),
                &core.process_engines.components().host().extensions,
                core.process_engines.engine_kinds(),
            )
            .map(Arc::new)
    }

    /// The claim requirements an engine process carries while it holds tool
    /// steps on `tools` (§6.2): its environment's components' config tokens
    /// and the revision tokens of the components those steps bind.
    pub(in crate::runtime) fn process_step_requirements(
        &self,
        tools: &[crate::ToolId],
    ) -> Result<Vec<String>, crate::PluginError> {
        let (_, engine) = self.current.engine_process()?;
        let mut requires =
            crate::plugin::ProcessComponentDeclaration::env_requirements(&engine.env);
        requires.extend(engine.composition.step_requirements(tools));
        requires.sort();
        requires.dedup();
        Ok(requires)
    }

    /// The tools `process`'s steps run (ADR 0132 §10): its catalog, the
    /// round tools that pin and run a catalog tool as a turn's round does,
    /// under `cx`, the process actor's claimed context. They run inside
    /// `process`: what a tool reads off the process it runs in, and what the
    /// children it starts inherit (its lineage, originator and environment),
    /// are the process's committed facts, read once when the activation
    /// builds its tools.
    ///
    /// # Errors
    ///
    /// The catalog does not resolve.
    pub(in crate::runtime) async fn process_step_tools(
        &self,
        cx: crate::ActorContext,
        process: &crate::ProcessRecord,
    ) -> Result<crate::runtime::ProcessStepTools, crate::PluginError> {
        use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
        let catalog = self.process_step_catalog()?;
        let (_, engine) = self.current.engine_process()?;
        let env = Arc::clone(&engine.env);
        let dispatch = self.process_step_dispatch(Arc::clone(&catalog), cx)?;
        let core = &self.current.host.core;
        let context = crate::RuntimeExecutionContext::new(
            dispatch,
            Arc::clone(&core.durability.process_env_store),
            Arc::clone(&core.durability.attachment_store),
            Arc::new(crate::ChronologicalProjection::default()),
            crate::TurnContext::default(),
        )
        .with_tool_fault_retry(core.control.pacing.tool_fault_retry)
        .with_turn_phase_probe(self.current.turn_phase_probe.clone())
        .with_fleet_format(core.session_store_factory().fleet_format())
        .with_tool_material_store(core.backend().tool_material_store())
        .with_process_work(self.current.host.work.process_wiring().cloned())
        .with_process_execution(process, Some(env), None);
        let tools = context
            .round_tools(crate::EffectOpener::Process {
                process_id: process.id.clone(),
            })
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        Ok(crate::runtime::ProcessStepTools { catalog, tools })
    }

    /// The dispatch an engine process's tool steps run on (ADR 0132 §10):
    /// its catalog, over the composition only its environment's components
    /// make, under `effect_controller`, the process actor's claimed context.
    /// The execution context that runs inside the process sets the lineage
    /// and originator its children start under (FIG-3607 R1).
    fn process_step_dispatch<'run>(
        &self,
        catalog: Arc<crate::ToolCatalog>,
        effect_controller: crate::ActorContext,
    ) -> Result<Arc<crate::tool_dispatch::ToolDispatchContext<'run>>, crate::PluginError> {
        let services = Arc::new(self.clone());
        let direct_completions = services.direct_completion_client(effect_controller.clone(), None);
        let (_, engine) = self.current.engine_process()?;
        let owner = self.current.execution_owner()?;
        let core = &self.current.host.core;
        Ok(Arc::new(crate::tool_dispatch::ToolDispatchContext {
            fleet_format: core.session_store_factory().fleet_format(),
            composition: Arc::clone(&engine.composition.composition),
            config: crate::plugin::OwnerConfigView::components(Arc::clone(&engine.env)),
            execution_budgets: core.control.execution_budgets.clone(),
            state: engine.state.clone(),
            tools: Arc::clone(&engine.composition.registry) as Arc<dyn crate::ToolProvider>,
            tool_registry: Some(Arc::clone(&engine.composition.registry)),
            tool_catalog: catalog,
            sessions: services.state_service(),
            session_lifecycle: services.lifecycle_service(),
            session_graph: services.graph_service(),
            processes: services.model_tool_process_service(),
            process_engines: services.process_engines().clone(),
            effect_controller,
            direct_completions,
            parent_invocation: None,
            observation_call_key: None,
            facts: crate::plugin::ExecutionFacts::of_process(&engine.env),
            owner,
            // A process's steps have no host lane: the dispatch observes
            // nowhere.
            observer: crate::engine::NullObservationSink::arc(),
            attachment_store: Arc::clone(&core.durability.attachment_store),
            turn_context: crate::TurnContext::default(),
            clock: Arc::clone(&core.clock),
            process_lineage: None,
            process_originator: None,
        }))
    }
}
