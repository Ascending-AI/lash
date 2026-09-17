use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ProtocolBuildInput {
    pub tool_catalog: Arc<crate::ToolCatalog>,
    pub plugin_extensions: crate::PluginExtensions,
    pub trigger_events: crate::TriggerEventCatalog,
    pub extra_prompt_contributions: Vec<crate::PromptContribution>,
}
