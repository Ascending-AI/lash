//! Which preparation callbacks need recorded tool outcomes.

use super::PluginSession;

impl PluginSession {
    /// Whether plain tool answers can ride the next model admission without
    /// a callback or stateful protocol consuming them first.
    pub fn can_defer_tool_outcomes(&self) -> bool {
        let hooks = &self.capabilities().contributions;
        let protocol = self.protocol_plugin_id();
        let catalog = self.prompt_catalog();
        !self.protocol_session().requires_recorded_tool_outcomes()
            && hooks.checkpoint_hooks.is_empty()
            && !self.has_after_turn_hooks()
            && !self.has_runtime_event_hooks()
            && !self.has_context_pressure_hooks()
            && !self.has_assistant_stream_hooks()
            && !self.has_assistant_stream_finished_hooks()
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
