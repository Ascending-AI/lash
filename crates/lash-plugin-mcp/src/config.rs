use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::McpError;

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_CALL_TIMEOUT_MS: u64 = 60_000;

fn default_startup_timeout_ms() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_MS
}

fn default_call_timeout_ms() -> u64 {
    DEFAULT_CALL_TIMEOUT_MS
}

fn is_default_startup_timeout_ms(value: &u64) -> bool {
    *value == DEFAULT_STARTUP_TIMEOUT_MS
}

fn is_default_call_timeout_ms(value: &u64) -> bool {
    *value == DEFAULT_CALL_TIMEOUT_MS
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Connection configuration for one MCP server. Tag (`transport`) selects
/// the wire transport; per-variant fields configure that transport.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerConfig {
    /// Spawn a child process and speak JSON-RPC over stdio.
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(
            default = "default_startup_timeout_ms",
            skip_serializing_if = "is_default_startup_timeout_ms"
        )]
        startup_timeout_ms: u64,
        #[serde(
            default = "default_call_timeout_ms",
            skip_serializing_if = "is_default_call_timeout_ms"
        )]
        call_timeout_ms: u64,
        /// Persist non-image MCP binary content as model attachments.
        #[serde(default, skip_serializing_if = "is_false")]
        binary_content_attachments: bool,
    },
    /// Newer MCP spec HTTP/JSON streaming transport.
    StreamableHttp {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(
            default = "default_startup_timeout_ms",
            skip_serializing_if = "is_default_startup_timeout_ms"
        )]
        startup_timeout_ms: u64,
        #[serde(
            default = "default_call_timeout_ms",
            skip_serializing_if = "is_default_call_timeout_ms"
        )]
        call_timeout_ms: u64,
        /// Persist non-image MCP binary content as model attachments.
        #[serde(default, skip_serializing_if = "is_false")]
        binary_content_attachments: bool,
    },
}

impl McpServerConfig {
    /// Convenience constructor for stdio servers.
    pub fn stdio(command: impl Into<String>, args: Vec<String>) -> Self {
        Self::Stdio {
            command: command.into(),
            args,
            env: BTreeMap::new(),
            cwd: None,
            startup_timeout_ms: default_startup_timeout_ms(),
            call_timeout_ms: default_call_timeout_ms(),
            binary_content_attachments: false,
        }
    }

    /// Convenience constructor for streamable-HTTP servers.
    pub fn streamable_http(url: impl Into<String>) -> Self {
        Self::StreamableHttp {
            url: url.into(),
            headers: BTreeMap::new(),
            startup_timeout_ms: default_startup_timeout_ms(),
            call_timeout_ms: default_call_timeout_ms(),
            binary_content_attachments: false,
        }
    }

    pub fn startup_timeout(&self) -> Duration {
        Duration::from_millis(match self {
            Self::Stdio {
                startup_timeout_ms, ..
            }
            | Self::StreamableHttp {
                startup_timeout_ms, ..
            } => *startup_timeout_ms,
        })
    }

    pub fn call_timeout(&self) -> Duration {
        Duration::from_millis(match self {
            Self::Stdio {
                call_timeout_ms, ..
            }
            | Self::StreamableHttp {
                call_timeout_ms, ..
            } => *call_timeout_ms,
        })
    }

    pub fn with_binary_content_attachments(mut self, enabled: bool) -> Self {
        match &mut self {
            Self::Stdio {
                binary_content_attachments,
                ..
            }
            | Self::StreamableHttp {
                binary_content_attachments,
                ..
            } => *binary_content_attachments = enabled,
        }
        self
    }

    pub(crate) fn binary_content_attachments(&self) -> bool {
        match self {
            Self::Stdio {
                binary_content_attachments,
                ..
            }
            | Self::StreamableHttp {
                binary_content_attachments,
                ..
            } => *binary_content_attachments,
        }
    }

    pub(crate) fn validate(&self, server_name: &str) -> Result<(), McpError> {
        if server_name.trim().is_empty() {
            return Err(McpError::Config(
                "MCP server name cannot be empty".to_string(),
            ));
        }
        if server_name.contains("__") {
            return Err(McpError::Config(format!(
                "MCP server `{server_name}` cannot contain `__`"
            )));
        }
        match self {
            Self::Stdio { command, .. } if command.trim().is_empty() => Err(McpError::Config(
                format!("MCP server `{server_name}` command cannot be empty"),
            )),
            Self::StreamableHttp { url, .. } if url.trim().is_empty() => Err(McpError::Config(
                format!("MCP server `{server_name}` URL cannot be empty"),
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_attachment_opt_in_is_explicit_and_defaults_off() {
        let default = McpServerConfig::stdio("mcp-server", Vec::new());
        assert!(!default.binary_content_attachments());
        let json = serde_json::to_value(&default).unwrap();
        assert!(json.get("binary_content_attachments").is_none());

        let enabled = default.with_binary_content_attachments(true);
        assert!(enabled.binary_content_attachments());
        assert_eq!(
            serde_json::to_value(enabled).unwrap()["binary_content_attachments"],
            true
        );
    }

    /// The accepted `transport` tags are exactly the two that can connect.
    /// A config naming any other transport — notably the legacy `sse` one this
    /// crate never supported — is rejected at deserialization, so an operator
    /// learns about it from their config rather than from a connect failure.
    #[test]
    fn only_stdio_and_streamable_http_transports_deserialize() {
        let stdio: McpServerConfig =
            serde_json::from_value(serde_json::json!({"transport": "stdio", "command": "srv"}))
                .expect("stdio config must deserialize");
        assert!(matches!(stdio, McpServerConfig::Stdio { .. }));

        let http: McpServerConfig = serde_json::from_value(
            serde_json::json!({"transport": "streamable_http", "url": "http://localhost:1/mcp"}),
        )
        .expect("streamable_http config must deserialize");
        assert!(matches!(http, McpServerConfig::StreamableHttp { .. }));

        for transport in ["sse", "http", "websocket"] {
            let err = serde_json::from_value::<McpServerConfig>(
                serde_json::json!({"transport": transport, "url": "http://localhost:1/mcp"}),
            )
            .expect_err("unsupported transport must be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("stdio") && msg.contains("streamable_http"),
                "rejection should name the supported transports: {msg}"
            );
        }
    }
}
