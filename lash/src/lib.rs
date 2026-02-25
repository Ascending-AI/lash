pub mod agent;
pub mod capabilities;
pub mod dynamic;
pub mod embedded;
pub mod instructions;
pub mod llm;
pub mod model_info;
pub mod oauth;
pub mod provider;
pub mod python_home;
pub mod runtime;
pub mod session;
#[cfg(feature = "sqlite-store")]
pub mod store;
pub mod tools;

// Re-exports
pub use agent::{
    Agent, AgentConfig, AgentEvent, Message, MessageRole, Part, PartKind, PromptOverrideMode,
    PromptSectionName, PromptSectionOverride, PruneState, TokenUsage,
};
pub use capabilities::{
    AgentCapabilities, CapabilityId, ResolvedFeatures, capability_def, capability_for_tool,
    default_enabled_capabilities, resolve_features,
};
pub use dynamic::{
    CapabilityProfile, DynamicCapabilityDef, DynamicStateSnapshot, DynamicToolProvider,
    DynamicToolSpec, InProcessToolExecutionAdapter, InProcessToolFuture, InProcessToolHandler,
    ReconfigureError, ResolvedProjection, ToolExecutionAdapter, agent_capabilities_from_profile,
    default_dynamic_capability_defs, profile_from_agent_capabilities, resolve_projection,
};
pub use instructions::{FsInstructionSource, InstructionLoader, InstructionSource};
pub use provider::{LashConfig, Provider};
pub use runtime::{
    AgentStateEnvelope, EventSink, InputItem, NoopEventSink, RunMode, RuntimeConfig, RuntimeEngine,
    TurnInput, TurnResult,
};
pub use session::{ExecResponse, Session, SessionError, UserPrompt};
#[cfg(feature = "sqlite-store")]
pub use store::{AgentState, Store, TaskEntry};
pub use tools::{ToolSet, ToolSetDeps};

/// A message sent from the sandbox to the host during execution.
#[derive(Clone, Debug)]
pub struct SandboxMessage {
    pub text: String,
    /// "final" or "tool_output"
    pub kind: String,
}

/// Sender for streaming progress messages from tools (e.g. live bash output).
pub type ProgressSender = tokio::sync::mpsc::UnboundedSender<SandboxMessage>;

/// A typed parameter for a tool definition.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolParam {
    pub name: String,
    /// Python type: "str", "int", "float", "bool", "list", "dict", "any"
    #[serde(default = "ToolParam::default_type")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default = "ToolParam::default_required")]
    pub required: bool,
}

impl ToolParam {
    fn default_type() -> String {
        "any".into()
    }
    fn default_required() -> bool {
        true
    }

    pub fn typed(name: &str, ty: &str) -> Self {
        Self {
            name: name.into(),
            r#type: ty.into(),
            description: String::new(),
            required: true,
        }
    }
    pub fn optional(name: &str, ty: &str) -> Self {
        Self {
            name: name.into(),
            r#type: ty.into(),
            description: String::new(),
            required: false,
        }
    }
}

/// A tool definition exposed to the Python REPL.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ToolParam>,
    /// Python return type: "str", "int", "float", "bool", "list", "dict", "any", "None"
    #[serde(
        default = "ToolDefinition::default_returns",
        skip_serializing_if = "String::is_empty"
    )]
    pub returns: String,
    /// Short usage examples for discovery UIs / REPL browsing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    /// Hidden tools are callable via `_call()` but don't appear in the REPL namespace or agent prompt.
    #[serde(default)]
    pub hidden: bool,
    /// Whether this tool should be injected into the LLM system prompt.
    /// Non-injected tools remain callable via discovery/meta helpers.
    #[serde(default)]
    pub inject_into_prompt: bool,
}

impl ToolDefinition {
    fn default_returns() -> String {
        "any".into()
    }

    /// Format as a typed Python signature: `name(param: type, ...) -> ret`
    pub fn signature(&self) -> String {
        let params: Vec<String> = self
            .params
            .iter()
            .map(|p| {
                let mut s = format!("{}: {}", p.name, p.r#type);
                if !p.required {
                    s.push_str(" = None");
                }
                s
            })
            .collect();
        let ret = if self.returns.is_empty() {
            "any"
        } else {
            &self.returns
        };
        format!("{}({}) -> {}", self.name, params.join(", "), ret)
    }

    /// Format all tools as a documentation block for LLM prompts.
    pub fn format_tool_docs(tools: &[ToolDefinition]) -> String {
        tools
            .iter()
            .map(|t| {
                let mut lines = format!("- `{}`", t.signature());
                if !t.description.is_empty() {
                    lines.push_str(&format!(" — {}", t.description));
                }
                // Include parameter descriptions if any have them
                for p in &t.params {
                    if !p.description.is_empty() {
                        lines.push_str(&format!("\n    - `{}`: {}", p.name, p.description));
                    }
                }
                lines
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// An image returned by a tool (e.g. read_file on a PNG).
#[derive(Clone, Debug)]
pub struct ToolImage {
    pub mime: String,
    pub data: Vec<u8>,
    pub label: String,
}

/// Result of executing a tool.
#[derive(Clone, Debug)]
pub struct ToolResult {
    pub success: bool,
    pub result: serde_json::Value,
    pub images: Vec<ToolImage>,
}

impl ToolResult {
    pub fn ok(result: serde_json::Value) -> Self {
        Self {
            success: true,
            result,
            images: vec![],
        }
    }
    pub fn err(result: serde_json::Value) -> Self {
        Self {
            success: false,
            result,
            images: vec![],
        }
    }
    pub fn err_fmt(msg: impl std::fmt::Display) -> Self {
        Self::err(serde_json::json!(msg.to_string()))
    }
    pub fn with_images(success: bool, result: serde_json::Value, images: Vec<ToolImage>) -> Self {
        Self {
            success,
            result,
            images,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ToolParam ──

    #[test]
    fn tool_param_typed() {
        let p = ToolParam::typed("name", "str");
        assert_eq!(p.name, "name");
        assert_eq!(p.r#type, "str");
        assert!(p.required);
    }

    #[test]
    fn tool_param_optional() {
        let p = ToolParam::optional("name", "int");
        assert_eq!(p.name, "name");
        assert_eq!(p.r#type, "int");
        assert!(!p.required);
    }

    // ── ToolDefinition::signature ──

    #[test]
    fn signature_required_params() {
        let td = ToolDefinition {
            name: "foo".into(),
            description: String::new(),
            params: vec![ToolParam::typed("x", "int"), ToolParam::typed("y", "str")],
            returns: "bool".into(),
            examples: vec![],
            hidden: false,
            inject_into_prompt: true,
        };
        assert_eq!(td.signature(), "foo(x: int, y: str) -> bool");
    }

    #[test]
    fn signature_optional_params() {
        let td = ToolDefinition {
            name: "bar".into(),
            description: String::new(),
            params: vec![ToolParam::optional("limit", "int")],
            returns: "list".into(),
            examples: vec![],
            hidden: false,
            inject_into_prompt: true,
        };
        assert_eq!(td.signature(), "bar(limit: int = None) -> list");
    }

    #[test]
    fn signature_empty_params() {
        let td = ToolDefinition {
            name: "noop".into(),
            description: String::new(),
            params: vec![],
            returns: "None".into(),
            examples: vec![],
            hidden: false,
            inject_into_prompt: true,
        };
        assert_eq!(td.signature(), "noop() -> None");
    }

    #[test]
    fn signature_empty_returns_defaults_to_any() {
        let td = ToolDefinition {
            name: "f".into(),
            description: String::new(),
            params: vec![],
            returns: String::new(),
            examples: vec![],
            hidden: false,
            inject_into_prompt: true,
        };
        assert_eq!(td.signature(), "f() -> any");
    }

    // ── format_tool_docs ──

    #[test]
    fn format_tool_docs_with_descriptions() {
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            params: vec![ToolParam {
                name: "path".into(),
                r#type: "str".into(),
                description: "File path".into(),
                required: true,
            }],
            returns: "str".into(),
            examples: vec![],
            hidden: false,
            inject_into_prompt: true,
        }];
        let docs = ToolDefinition::format_tool_docs(&tools);
        assert!(docs.contains("- `read(path: str) -> str`"));
        assert!(docs.contains("— Read a file"));
        assert!(docs.contains("- `path`: File path"));
    }

    #[test]
    fn format_tool_docs_empty() {
        let docs = ToolDefinition::format_tool_docs(&[]);
        assert!(docs.is_empty());
    }
}

/// Record of a tool call (for context/logging).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolCallRecord {
    pub tool: String,
    pub args: serde_json::Value,
    pub result: serde_json::Value,
    pub success: bool,
    pub duration_ms: u64,
}

/// Trait for providing tools to the sandbox. Implement this per-project.
#[async_trait::async_trait]
pub trait ToolProvider: Send + Sync + 'static {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn dynamic_projection(&self) -> Option<crate::dynamic::ResolvedProjection> {
        None
    }
    fn dynamic_snapshot(&self) -> Option<crate::dynamic::DynamicStateSnapshot> {
        None
    }
    fn fork_dynamic_with_snapshot(
        &self,
        _snapshot: crate::dynamic::DynamicStateSnapshot,
    ) -> Option<std::sync::Arc<dyn ToolProvider>> {
        None
    }
    fn dynamic_capabilities_payload_json(&self) -> Option<String> {
        None
    }
    fn dynamic_generation(&self) -> Option<u64> {
        None
    }
    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult;

    /// Execute with progress streaming. Default: delegates to execute().
    async fn execute_streaming(
        &self,
        name: &str,
        args: &serde_json::Value,
        _progress: Option<&ProgressSender>,
    ) -> ToolResult {
        self.execute(name, args).await
    }
}
