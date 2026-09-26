use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;

mod control;
mod runner;
mod session;
mod tool;

pub(in crate::runtime::session_manager::process_runners) struct ProcessRunContext<'run> {
    dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    /// The process incarnation's live-opener registration (ADR 0099 §3), when
    /// this host routes tool children and the scope names an opener. Held here
    /// so `shutdown` releases it with the dispatch: the lent context's
    /// observation sink is the process's own, so nothing the registration
    /// held could outlive them.
    live_opener: Option<crate::LiveOpenerGuard>,
}

impl<'run> ProcessRunContext<'run> {
    pub(in crate::runtime::session_manager::process_runners) fn builder(
        services: &RuntimeSessionServices,
    ) -> ProcessRunContextBuilder<'_, 'run> {
        ProcessRunContextBuilder {
            services,
            tool_surface: None,
            scoped_effect_controller: None,
            causal_invocation: None,
            dispatch_parent_invocation: None,
            cancellation: tokio_util::sync::CancellationToken::new(),
            process_lineage: None,
        }
    }

    pub(in crate::runtime::session_manager::process_runners) fn dispatch(
        &self,
    ) -> Arc<crate::tool_dispatch::ToolDispatchContext<'run>> {
        Arc::clone(&self.dispatch)
    }

    pub(in crate::runtime::session_manager::process_runners) async fn shutdown(self) {
        let Self {
            dispatch,
            live_opener,
        } = self;
        drop(live_opener);
        drop(dispatch);
    }
}

pub(in crate::runtime::session_manager::process_runners) struct ProcessRunContextBuilder<'a, 'run> {
    services: &'a RuntimeSessionServices,
    tool_surface: Option<crate::plugin::ResolvedToolSurface>,
    scoped_effect_controller: Option<crate::ScopedEffectController<'run>>,
    causal_invocation: Option<crate::RuntimeInvocation>,
    dispatch_parent_invocation: Option<crate::RuntimeInvocation>,
    cancellation: tokio_util::sync::CancellationToken,
    process_lineage: Option<crate::ProcessLineage>,
}

pub(in crate::runtime::session_manager::process_runners) struct ProcessToolCallRun<'run> {
    process_id: crate::ProcessId,
    lineage: crate::ProcessLineage,
    call: crate::PreparedToolCall,
    parent_invocation: Option<crate::RuntimeInvocation>,
    execution_write_authority: crate::ProcessExecutionWriteAuthority,
    scoped_effect_controller: crate::ScopedEffectController<'run>,
    cancellation: tokio_util::sync::CancellationToken,
}

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
        scoped_effect_controller: crate::ScopedEffectController<'run>,
    ) -> Self {
        self.scoped_effect_controller = Some(scoped_effect_controller);
        self
    }

    pub(in crate::runtime::session_manager::process_runners) fn dispatch_parent_invocation(
        mut self,
        invocation: Option<crate::RuntimeInvocation>,
    ) -> Self {
        self.dispatch_parent_invocation = invocation;
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

    /// The cooperative signal the lent opener context carries: a tool child's
    /// waits cancel with the process that opened it (FIG-2266).
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
        // Derive the opener before the controller moves into the handle. The
        // derivation is the one owner derivation (`EffectOpener::for_scope`,
        // FIG-3417): the admitted scope plus its pinned incarnation, never a
        // registry lookup — a process scope without one names no opener and
        // registers nothing, leaving its children accepted rather than run
        // under a context that cannot claim them.
        let opener = crate::facade_support::opener_for_execution_scope(
            scoped_effect_controller.admitted_scope(),
        );
        let effect_controller =
            crate::runtime::RuntimeEffectControllerHandle::borrowed(scoped_effect_controller);
        let direct_completions = services.direct_completion_client(
            effect_controller.clone_scoped(),
            self.causal_invocation
                .as_ref()
                .and_then(|invocation| invocation.attribution.turn_id.clone()),
        );
        let state = self.services.current.snapshot.to_runtime_state();
        let execution_env_spec = state.process_execution_env_spec(&self.services.current.policy);
        let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
            plugins: Arc::clone(&self.services.current.plugins),
            tools: Arc::clone(&tool_surface.registry) as Arc<dyn crate::ToolProvider>,
            tool_registry: Some(Arc::clone(&tool_surface.registry)),
            tool_catalog: tool_surface.catalog,
            sessions: services.state_service(),
            session_lifecycle: services.lifecycle_service(),
            session_graph: services.graph_service(),
            processes: services.model_tool_process_service(),
            trigger_router: services.trigger_router(),
            process_definitions: services.process_definition_registry(),
            process_engines: services.process_engines().clone(),
            effect_controller,
            direct_completions,
            parent_invocation: self.dispatch_parent_invocation,
            observation_call_key: None,
            execution_env_spec,
            session_id: self.services.current.session_id.clone(),
            agent_frame_id: state.current_frame_node_id.clone().ok_or_else(|| {
                crate::PluginError::Session(
                    "process execution requires an initialized agent frame".to_string(),
                )
            })?,
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
        });
        // Publish the process incarnation as a live opener, lending this
        // dispatch context to the group children it opens (ADR 0099 §3). The
        // lent context keeps the dispatch's own observation sink — nowhere —
        // so nothing a child emits outlives the registration. Nothing
        // registers when the deployment routes no tool children, the scope
        // names no opener, or the host hands out no owned controller to lend
        // the captured context's controller slots — the lend a `'static`
        // capture needs, since the runner's own live controller cannot
        // outlive its frame.
        let live_opener = opener
            .zip(
                self.services
                    .current
                    .host
                    .core
                    .control
                    .tool_children
                    .as_ref(),
            )
            .and_then(|(opener, tool_children)| {
                let lent_controller = self
                    .services
                    .current
                    .host
                    .core
                    .control
                    .effect_host
                    .scoped_static(dispatch.effect_controller.scoped().admitted_scope().clone())
                    .ok()??;
                let context = crate::facade_support::LiveOpenerContext::capture(
                    dispatch.as_ref(),
                    lent_controller,
                    self.cancellation.clone(),
                );
                let (guard, _ended) = tool_children.openers().register(opener, context);
                Some(guard)
            });
        Ok(ProcessRunContext {
            dispatch,
            live_opener,
        })
    }
}
