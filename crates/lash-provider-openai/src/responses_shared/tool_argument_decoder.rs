use std::collections::HashMap;

use lash_core::facade_support::{
    ProviderSchemaCapabilities, SchemaPurpose, SchemaResolutionRequest, resolve_schema,
};
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::LlmRequest;
use lash_sansio::{OmissionNullPath, OmissionNullPathSegment};
use serde_json::Value;

use super::projection_error;

/// Decodes completed provider tool-call arguments using only omission paths
/// recorded by this request's schema projection.
#[derive(Clone, Debug, Default)]
pub struct ToolArgumentDecoder {
    omission_paths_by_tool: HashMap<String, Vec<OmissionNullPath>>,
}

impl ToolArgumentDecoder {
    pub fn for_request(
        provider: &str,
        req: &LlmRequest,
        strict_tools: bool,
        capabilities: &ProviderSchemaCapabilities,
    ) -> Result<Self, LlmTransportError> {
        if !strict_tools {
            return Ok(Self::default());
        }
        let mut omission_paths_by_tool = HashMap::new();
        for tool in req.tools.iter() {
            let resolved = resolve_schema(
                &tool.input_schema,
                SchemaResolutionRequest {
                    provider,
                    purpose: SchemaPurpose::ToolInput,
                    dialects: capabilities.dialects_for(SchemaPurpose::ToolInput),
                },
            )
            .map_err(|error| projection_error(provider, error))?;
            if !resolved.omission_null_paths.is_empty() {
                omission_paths_by_tool.insert(tool.name.clone(), resolved.omission_null_paths);
            }
        }
        Ok(Self {
            omission_paths_by_tool,
        })
    }

    pub fn decode(&self, tool_name: &str, input_json: String) -> String {
        let Some(paths) = self.omission_paths_by_tool.get(tool_name) else {
            return input_json;
        };
        let Ok(mut arguments) = serde_json::from_str::<Value>(&input_json) else {
            return input_json;
        };
        if !arguments.is_object() {
            return input_json;
        }
        let mut changed = false;
        for path in paths {
            changed = remove_omission_null(&mut arguments, path.segments()) || changed;
        }
        if !changed {
            return input_json;
        }
        serde_json::to_string(&arguments).unwrap_or(input_json)
    }
}

fn remove_omission_null(value: &mut Value, path: &[OmissionNullPathSegment]) -> bool {
    let Some((segment, remaining)) = path.split_first() else {
        return false;
    };
    match segment {
        OmissionNullPathSegment::Property(name) => {
            let Some(object) = value.as_object_mut() else {
                return false;
            };
            if remaining.is_empty() {
                if object.get(name).is_some_and(Value::is_null) {
                    object.remove(name);
                    return true;
                }
                return false;
            }
            object
                .get_mut(name)
                .is_some_and(|child| remove_omission_null(child, remaining))
        }
        OmissionNullPathSegment::ArrayItem => value.as_array_mut().is_some_and(|items| {
            let mut changed = false;
            for item in items {
                changed = remove_omission_null(item, remaining) || changed;
            }
            changed
        }),
    }
}
