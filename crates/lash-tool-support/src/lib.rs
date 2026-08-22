//! Shared implementation support for Lash tools.

use lash_core::{ToolDefinition, ToolFailure, ToolFailureClass, ToolOutcome};

mod static_provider;
#[cfg(feature = "lashlang")]
pub use lash_lashlang_runtime::ToolBinding;
pub use static_provider::{StaticToolExecute, StaticToolProvider};

#[cfg(not(feature = "lashlang"))]
#[derive(Clone, Debug, Default)]
pub struct ToolBinding;

#[cfg(not(feature = "lashlang"))]
impl ToolBinding {
    pub fn new(
        module_path: impl IntoIterator<Item = impl Into<String>>,
        operation: impl Into<String>,
    ) -> Self {
        let _ = module_path
            .into_iter()
            .map(Into::into)
            .collect::<Vec<String>>();
        let _ = operation.into();
        Self
    }

    pub fn with_authority_type(self, authority_type: impl Into<String>) -> Self {
        let _ = authority_type.into();
        self
    }

    pub fn with_aliases(self, aliases: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let _ = aliases.into_iter().map(Into::into).collect::<Vec<String>>();
        self
    }
}

pub trait ToolDefinitionBindingExt {
    fn with_tool_binding(self, tool_binding: ToolBinding) -> Self;
}

#[cfg(feature = "lashlang")]
impl ToolDefinitionBindingExt for ToolDefinition {
    fn with_tool_binding(self, tool_binding: ToolBinding) -> Self {
        lash_lashlang_runtime::ToolDefinitionBindingExt::with_tool_binding(self, tool_binding)
    }
}

#[cfg(not(feature = "lashlang"))]
impl ToolDefinitionBindingExt for ToolDefinition {
    fn with_tool_binding(self, _tool_binding: ToolBinding) -> Self {
        self
    }
}

pub fn invalid_request_failure(code: impl Into<String>, message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::invalid_request(code, message))
}

pub fn retryable_io_failure(
    code: impl Into<String>,
    message: impl Into<String>,
    after_ms: Option<u64>,
) -> ToolOutcome {
    ToolOutcome::retryable_failure(ToolFailureClass::Io, code, message, after_ms)
}

pub fn execution_failure(code: impl Into<String>, message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::tool(
        ToolFailureClass::Execution,
        code,
        message,
    ))
}

pub fn invalid_tool_args(message: impl Into<String>) -> ToolOutcome {
    invalid_request_failure("invalid_tool_args", message)
}

/// Extract a required non-empty string arg, or return a structured invalid request.
pub fn require_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolOutcome> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_tool_args(format!("Missing required parameter: {key}")))
}

/// Parse optional bool arg with a default.
pub fn parse_optional_bool(
    args: &serde_json::Value,
    key: &str,
    default: bool,
) -> Result<bool, ToolOutcome> {
    match args.get(key) {
        None => Ok(default),
        Some(v) if v.is_null() => Ok(default),
        Some(v) => match v.as_bool() {
            Some(b) => Ok(b),
            None => Err(invalid_tool_args(format!("Invalid {key}: expected bool"))),
        },
    }
}

/// Parse an optional positive integer arg.
/// Accepts `null` or `"none"` when `allow_none` is true.
pub fn parse_optional_usize_arg(
    args: &serde_json::Value,
    key: &str,
    default: Option<usize>,
    allow_none: bool,
    min: usize,
) -> Result<Option<usize>, ToolOutcome> {
    match args.get(key) {
        None => Ok(default),
        Some(v) if v.is_null() => {
            if allow_none {
                Ok(None)
            } else {
                Err(invalid_tool_args(format!(
                    "Invalid {key}: expected int >= {min}"
                )))
            }
        }
        Some(v) => {
            if let Some(s) = v.as_str() {
                if allow_none && s.eq_ignore_ascii_case("none") {
                    return Ok(None);
                }
                return Err(invalid_tool_args(format!(
                    "Invalid {key}: expected int{}",
                    if allow_none {
                        ", null, or \"none\""
                    } else {
                        ""
                    }
                )));
            }
            let n = v.as_u64().ok_or_else(|| {
                invalid_tool_args(format!(
                    "Invalid {key}: expected int{}",
                    if allow_none {
                        ", null, or \"none\""
                    } else {
                        ""
                    }
                ))
            })? as usize;
            if n < min {
                return Err(invalid_tool_args(format!(
                    "Invalid {key}: must be >= {min}{}",
                    if allow_none {
                        ", or use null/\"none\" for no cap"
                    } else {
                        ""
                    }
                )));
            }
            Ok(Some(n))
        }
    }
}

pub fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn tool_binding(
    module_path: impl IntoIterator<Item = impl Into<String>>,
    operation: impl Into<String>,
    aliases: &[&str],
) -> ToolBinding {
    ToolBinding::new(module_path, operation).with_aliases(aliases.iter().copied())
}
