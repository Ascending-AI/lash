//! The dispatch context a group tool child runs under when the deployment
//! builds it (FIG-3712): the session's own wiring, with no opener to lend it.

use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;

impl RuntimeSessionServices {
    /// This session's tool-execution context for a group tool child whose
    /// opener is not live where the child runs.
    ///
    /// Built from the session's wiring the way a process run context is: its
    /// plugin session and pinned tool surface, its session services, its
    /// process, trigger and attachment wiring, its provider. The child's
    /// recorded facts are bound over it by the tool-child driver's rebind, and
    /// its observer is replaced by the driver's recorder, so the observer
    /// given here is never read. `lent_controller` fills the
    /// controller slots the rebind replaces.
    pub(in crate::runtime) fn tool_child_dispatch(
        self: &Arc<Self>,
        lent_controller: crate::ScopedEffectController<'static>,
    ) -> Result<crate::tool_dispatch::ToolDispatchContext<'static>, crate::PluginError> {
        let tool_surface = self
            .current
            .plugins
            .pin_resolved_tool_surface(&self.current.session_id)?;
        let effect_controller =
            crate::runtime::RuntimeEffectControllerHandle::borrowed(lent_controller);
        let direct_completions =
            self.direct_completion_client(effect_controller.clone_scoped(), None);
        let state = self.current.snapshot.to_runtime_state();
        let execution_env_spec = state.process_execution_env_spec(&self.current.policy);
        // Never read: the driver points the observer at its recorder.
        let observer = crate::engine::NullObservationSink::arc();
        Ok(crate::tool_dispatch::ToolDispatchContext {
            plugins: Arc::clone(&self.current.plugins),
            tools: Arc::clone(&tool_surface.registry) as Arc<dyn crate::ToolProvider>,
            tool_registry: Some(Arc::clone(&tool_surface.registry)),
            tool_catalog: tool_surface.catalog,
            sessions: self.state_service(),
            session_lifecycle: self.lifecycle_service(),
            session_graph: self.graph_service(),
            processes: self.model_tool_process_service(),
            trigger_router: self.trigger_router(),
            process_definitions: self.process_definition_registry(),
            process_engines: self.process_engines().clone(),
            effect_controller,
            direct_completions,
            parent_invocation: None,
            observation_call_key: None,
            execution_env_spec,
            session_id: self.current.session_id.clone(),
            agent_frame_id: state.current_frame_node_id.clone().ok_or_else(|| {
                crate::PluginError::Session(
                    "a tool child's session has no initialized agent frame".to_string(),
                )
            })?,
            observer,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&self.current.host.core.durability.attachment_store),
            attachment_source_policy: Arc::clone(&self.current.host.core.attachment_source_policy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::clone(&self.current.host.core.clock),
            process_lineage: None,
        })
    }
}
