//! Bind host tools to dialect call paths.

use super::{TOOL_BINDING_KEY, ToolDefinition, ToolDraft};

/// Dialect-agnostic binding that makes a host tool callable as a module
/// operation during code-mode execution. Which dialect executes the call is
/// lash's decision; the host supplies the module path, operation, and optional
/// authority type and aliases.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolBinding {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl ToolBinding {
    pub fn new(
        module_path: impl IntoIterator<Item = impl Into<String>>,
        operation: impl Into<String>,
    ) -> Self {
        Self {
            module_path: module_path.into_iter().map(Into::into).collect(),
            operation: Some(operation.into()),
            authority_type: None,
            aliases: Vec::new(),
        }
    }

    /// Bind a dotted call path, with its last segment as the operation.
    /// Dialects validate whether the resulting path is callable.
    pub fn from_path(path: &str) -> Self {
        let mut segments: Vec<_> = path.split('.').map(str::to_owned).collect();
        Self {
            operation: segments.pop(),
            module_path: segments,
            authority_type: None,
            aliases: Vec::new(),
        }
    }

    pub fn with_authority_type(mut self, authority_type: impl Into<String>) -> Self {
        self.authority_type = Some(authority_type.into());
        self
    }

    pub fn with_aliases(mut self, aliases: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.aliases = aliases.into_iter().map(Into::into).collect();
        self
    }
}

/// The one host-facing setter for a tool's executable binding.
///
/// The manifest key the binding is written under
/// (`lash.tool`) is lash's own projection onto the opaque
/// `bindings` map: hosts never read or write it, and no dialect choice is
/// exposed or implied by calling this setter.
pub trait ToolDefinitionBindingExt {
    fn with_tool_binding(self, tool_binding: ToolBinding) -> Self;
}

impl ToolDefinitionBindingExt for ToolDraft {
    #[expect(
        clippy::expect_used,
        reason = "ToolBinding is a module-owned struct of strings and maps, so serialization into the manifest's JSON bindings map can only fail if the type is widened, which the site's message asserts"
    )]
    fn with_tool_binding(mut self, tool_binding: ToolBinding) -> Self {
        self.bindings.insert(
            TOOL_BINDING_KEY.to_string(),
            serde_json::to_value(&tool_binding).expect("tool binding must serialize to JSON"),
        );
        self
    }
}

impl ToolDefinitionBindingExt for ToolDefinition {
    #[expect(
        clippy::expect_used,
        reason = "ToolBinding is a module-owned struct of strings and maps, so serialization into the manifest's JSON bindings map can only fail if the type is widened, which the site's message asserts"
    )]
    fn with_tool_binding(mut self, tool_binding: ToolBinding) -> Self {
        self.manifest.bindings.insert(
            TOOL_BINDING_KEY.to_string(),
            serde_json::to_value(&tool_binding).expect("tool binding must serialize to JSON"),
        );
        self
    }
}
