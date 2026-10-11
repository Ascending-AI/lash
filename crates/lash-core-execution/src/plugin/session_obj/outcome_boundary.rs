//! Which preparation callbacks need recorded tool outcomes.

use super::PluginSession;

impl PluginSession {
    /// Whether plain tool answers can ride the next model admission without
    /// a callback or stateful protocol consuming them first.
    pub fn can_defer_tool_outcomes(&self) -> bool {
        let hooks = &self.capabilities().contributions;
        !self.protocol_session().requires_recorded_tool_outcomes()
            && hooks.checkpoint_hooks.is_empty()
            && !self.has_assistant_stream_hooks()
            && !self.has_assistant_stream_finished_hooks()
            && self.only_the_protocol_prepares()
    }

    /// Whether a cell's end can ride the commit that consumes its answer
    /// (FIG-5999): no callback but the protocol's own runs between the two.
    /// A protocol's callbacks read the cell from the session's live state,
    /// never from the store, so they need no recorded end.
    pub fn can_hold_cell_end(&self) -> bool {
        let hooks = &self.capabilities().contributions;
        let protocol = self.protocol_plugin_id();
        hooks
            .checkpoint_hooks
            .iter()
            .all(|hook| hook.identity.owner.plugin == protocol)
            && hooks
                .assistant_stream_hooks
                .iter()
                .all(|hook| hook.identity.owner.plugin == protocol)
            && hooks
                .assistant_stream_finished_hooks
                .iter()
                .all(|hook| hook.identity.owner.plugin == protocol)
            && self.only_the_protocol_prepares()
    }

    /// No after-turn, runtime-event or context-pressure callback, and no
    /// tool catalog or prompt contribution but the protocol's own.
    fn only_the_protocol_prepares(&self) -> bool {
        let hooks = &self.capabilities().contributions;
        let protocol = self.protocol_plugin_id();
        let catalog = self.prompt_catalog();
        !self.has_after_turn_hooks()
            && !self.has_runtime_event_hooks()
            && !self.has_context_pressure_hooks()
            && hooks
                .tool_catalog_contributors
                .iter()
                .all(|hook| hook.identity.owner.plugin == protocol)
            && catalog
                .sections()
                .iter()
                .all(|section| section.owner.plugin == protocol)
            && catalog
                .families()
                .iter()
                .all(|family| family.owner.plugin == protocol)
            && catalog
                .wraps()
                .iter()
                .all(|wrap| wrap.owner.plugin == protocol)
    }

    pub fn has_runtime_event_hooks(&self) -> bool {
        !self
            .capabilities()
            .contributions
            .runtime_event_hooks
            .is_empty()
    }
    pub fn has_assistant_stream_hooks(&self) -> bool {
        !self
            .capabilities()
            .contributions
            .assistant_stream_hooks
            .is_empty()
    }
    pub fn has_assistant_stream_finished_hooks(&self) -> bool {
        !self
            .capabilities()
            .contributions
            .assistant_stream_finished_hooks
            .is_empty()
    }
    pub fn has_context_pressure_hooks(&self) -> bool {
        !self
            .capabilities()
            .contributions
            .context_pressure_hooks
            .is_empty()
    }
    pub(crate) fn has_tool_result_hooks(&self) -> bool {
        !self
            .capabilities()
            .contributions
            .tool_result_transforms
            .is_empty()
            || !self
                .capabilities()
                .contributions
                .tool_result_checks
                .is_empty()
    }
}
