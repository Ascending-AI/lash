use super::*;

#[cfg(test)]
mod context_tests;
mod control;
mod runner;
mod session;

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "L6 (FIG-5175) drives engine processes by advance and runs their steps on this dispatch wiring; ProcessEngine::run, its only caller, is deleted (I0)"
    )
)]
pub(in crate::runtime::session_manager::process_runners) struct ProcessRunContext<'run> {
    dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "L6 (FIG-5175) drives engine processes by advance and runs their steps on this dispatch wiring; ProcessEngine::run, its only caller, is deleted (I0)"
    )
)]
impl<'run> ProcessRunContext<'run> {
    pub(in crate::runtime::session_manager::process_runners) fn builder(
        services: &RuntimeSessionServices,
    ) -> ProcessRunContextBuilder<'_, 'run> {
        ProcessRunContextBuilder {
            run: std::marker::PhantomData,
            services,
            tool_surface: None,
            scoped_effect_controller: None,
            causal_invocation: None,
            cancellation: tokio_util::sync::CancellationToken::new(),
            process_lineage: None,
            process_originator: None,
        }
    }

    pub(in crate::runtime::session_manager::process_runners) fn dispatch(
        &self,
    ) -> Arc<crate::tool_dispatch::ToolDispatchContext<'run>> {
        Arc::clone(&self.dispatch)
    }

    pub(in crate::runtime::session_manager::process_runners) async fn shutdown(self) {
        let Self { dispatch } = self;
        drop(dispatch);
    }
}

#[expect(
    dead_code,
    reason = "L6 (FIG-5175) drives engine processes by advance and runs their steps on this dispatch wiring; ProcessEngine::run, its only caller, is deleted (I0)"
)]
pub(in crate::runtime::session_manager::process_runners) struct ProcessRunContextBuilder<'a, 'run> {
    services: &'a RuntimeSessionServices,
    tool_surface: Option<crate::plugin::ResolvedToolSurface>,
    scoped_effect_controller: Option<crate::ActorContext>,
    causal_invocation: Option<crate::RuntimeInvocation>,
    cancellation: tokio_util::sync::CancellationToken,
    process_lineage: Option<crate::ProcessLineage>,
    process_originator: Option<crate::ProcessOriginator>,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(crate) run: std::marker::PhantomData<&'run ()>,
}

#[expect(
    dead_code,
    reason = "L6 (FIG-5175) drives engine processes by advance and runs their steps on this dispatch wiring; ProcessEngine::run, its only caller, is deleted (I0)"
)]
impl<'a, 'run> ProcessRunContextBuilder<'a, 'run> {
    pub(in crate::runtime::session_manager::process_runners) fn tool_surface(
        mut self,
        tool_surface: crate::plugin::ResolvedToolSurface,
    ) -> Self {
        self.tool_surface = Some(tool_surface);
        self
    }

    pub(in crate::runtime::session_manager::process_runners) fn causal_invocation(
        mut self,
        invocation: Option<crate::RuntimeInvocation>,
    ) -> Self {
        self.causal_invocation = invocation;
        self
    }

    pub(in crate::runtime::session_manager::process_runners) fn scoped_effect_controller(
        mut self,
        scoped_effect_controller: crate::ActorContext,
    ) -> Self {
        self.scoped_effect_controller = Some(scoped_effect_controller);
        self
    }

    /// The lineage of the process this context runs, which every start made
    /// inside it records above its starter (FIG-3607 R1).
    pub(in crate::runtime::session_manager::process_runners) fn process_lineage(
        mut self,
        lineage: crate::ProcessLineage,
    ) -> Self {
        self.process_lineage = Some(lineage);
        self
    }

    pub(in crate::runtime::session_manager::process_runners) fn process_originator(
        mut self,
        originator: crate::ProcessOriginator,
    ) -> Self {
        self.process_originator = Some(originator);
        self
    }

    /// Cooperative cancellation shared by this process's recorded tool bodies.
    pub(in crate::runtime::session_manager::process_runners) fn cancellation(
        mut self,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub(in crate::runtime::session_manager::process_runners) fn build(
        self,
    ) -> Result<ProcessRunContext<'run>, crate::PluginError> {
        let tool_surface = self.tool_surface.ok_or_else(|| {
            crate::PluginError::Session("process run context requires a tool surface".to_string())
        })?;
        let services = Arc::new(self.services.clone());
        let scoped_effect_controller = self.scoped_effect_controller.ok_or_else(|| {
            crate::PluginError::Session(
                "process run context requires a scoped effect controller".to_string(),
            )
        })?;
        let effect_controller = scoped_effect_controller;
        let direct_completions = services.direct_completion_client(
            effect_controller.clone(),
            self.causal_invocation
                .as_ref()
                .and_then(|invocation| invocation.attribution.turn_id.clone()),
        );
        let execution_env_spec = self.services.current.execution_env_spec()?;
        let owner = self.services.current.execution_owner()?;
        let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
            tool_receipts: Some(self.services.current.host.core.session_store_factory()),
            plugins: Arc::clone(&self.services.current.plugins),
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
            // A process run has no host lane: its emissions went to a
            // drained channel before, so the dispatch observes nowhere.
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(
                &self.services.current.host.core.durability.attachment_store,
            ),
            attachment_source_policy: Arc::clone(
                &self.services.current.host.core.attachment_source_policy,
            ),
            turn_context: crate::TurnContext::default(),
            clock: Arc::clone(&self.services.current.host.core.clock),
            process_lineage: self.process_lineage,
            process_originator: self.process_originator,
        });
        Ok(ProcessRunContext { dispatch })
    }
}
