use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;

mod control;
mod runner;
mod session;
mod tool;

#[cfg(any(test, feature = "testing"))]
pub(crate) use control::guard_process_command_in_recorded_body;

pub(in crate::runtime::session_manager::process_runners) struct ProcessRunContext<'run> {
    dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    event_drain: tokio::task::JoinHandle<()>,
}

impl<'run> ProcessRunContext<'run> {
    pub(in crate::runtime::session_manager::process_runners) fn builder(
        services: &RuntimeSessionServices,
    ) -> ProcessRunContextBuilder<'_, 'run> {
        ProcessRunContextBuilder {
            services,
            tool_catalog: None,
            scoped_effect_controller: None,
            causal_invocation: None,
            dispatch_parent_invocation: None,
        }
    }

    pub(in crate::runtime::session_manager::process_runners) fn dispatch(
        &self,
    ) -> Arc<crate::tool_dispatch::ToolDispatchContext<'run>> {
        Arc::clone(&self.dispatch)
    }

    pub(in crate::runtime::session_manager::process_runners) async fn shutdown(self) {
        drop(self.dispatch);
        let _ = self.event_drain.await;
    }
}

pub(in crate::runtime::session_manager::process_runners) struct ProcessRunContextBuilder<'a, 'run> {
    services: &'a RuntimeSessionServices,
    tool_catalog: Option<Arc<crate::ToolCatalog>>,
    scoped_effect_controller: Option<crate::ScopedEffectController<'run>>,
    causal_invocation: Option<crate::RuntimeInvocation>,
    dispatch_parent_invocation: Option<crate::RuntimeInvocation>,
}

pub(in crate::runtime::session_manager::process_runners) struct ProcessToolCallRun<'run> {
    registration: crate::ProcessRegistration,
    registry: Arc<dyn crate::ProcessRegistry>,
    call: crate::PreparedToolCall,
    parent_invocation: Option<crate::RuntimeInvocation>,
    execution_write_authority: crate::ProcessExecutionWriteAuthority,
    scoped_effect_controller: crate::ScopedEffectController<'run>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl<'a, 'run> ProcessRunContextBuilder<'a, 'run> {
    pub(in crate::runtime::session_manager::process_runners) fn tool_catalog(
        mut self,
        tool_catalog: Arc<crate::ToolCatalog>,
    ) -> Self {
        self.tool_catalog = Some(tool_catalog);
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

    pub(in crate::runtime::session_manager::process_runners) fn build(
        self,
    ) -> Result<ProcessRunContext<'run>, crate::PluginError> {
        let tool_catalog = self.tool_catalog.ok_or_else(|| {
            crate::PluginError::Session("process run context requires a tool catalog".to_string())
        })?;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<crate::SessionStreamEvent>(64);
        let event_drain =
            crate::task::spawn(async move { while event_rx.recv().await.is_some() {} });
        let services = Arc::new(self.services.clone());
        let scoped_effect_controller = self.scoped_effect_controller.ok_or_else(|| {
            crate::PluginError::Session(
                "process run context requires a scoped effect controller".to_string(),
            )
        })?;
        let effect_controller =
            crate::runtime::RuntimeEffectControllerHandle::borrowed(scoped_effect_controller);
        let direct_completions = services.direct_completion_client(
            effect_controller.clone_scoped(),
            self.causal_invocation
                .as_ref()
                .and_then(|invocation| invocation.scope.turn_id.clone()),
        );
        let state = self.services.current.snapshot.to_runtime_state();
        let execution_env_spec = state.process_execution_env_spec(&self.services.current.policy);
        let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
            plugins: Arc::clone(&self.services.current.plugins),
            tools: self.services.current.plugins.tools(),
            tool_catalog,
            sessions: services.state_service(),
            session_lifecycle: services.lifecycle_service(),
            session_graph: services.graph_service(),
            processes: services.model_tool_process_service(),
            trigger_router: services.trigger_router(),
            effect_controller,
            direct_completions,
            parent_invocation: self.dispatch_parent_invocation,
            execution_env_spec,
            session_id: self.services.current.session_id.clone(),
            agent_frame_id: String::new(),
            event_tx,
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
        });
        Ok(ProcessRunContext {
            dispatch,
            event_drain,
        })
    }
}
