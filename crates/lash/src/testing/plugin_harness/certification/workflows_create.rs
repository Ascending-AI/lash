//! The model-facing definition for host workflow creation. Attachment and
//! publication belong to the host's explicit linked workflow request.
use crate::plugins::*;
use crate::tools::*;

pub(super) fn definition() -> Result<ToolDefinition, PluginError> {
    ToolDefinition::raw(
        "host:workflows_create", "workflows.create", "Create a workflow from its code.",
        serde_json::json!({"type":"object", "properties":{"code":{"type":"string"}}, "required":["code"], "additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    ).map(|draft| draft.with_execution(std::time::Duration::from_secs(120)))
        .map_err(|error| PluginError::Registration(error.to_string()))
}
