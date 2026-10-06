use crate::pool::McpServerHealth;
use lash_core::{
    AttachmentRetentionFailure, ToolFailure, ToolFailureClass, ToolFailureSource, ToolOutcome,
    ToolValue,
};
use rmcp::{ServiceError, model::ErrorData};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Typed evidence recorded in ToolFailure.raw, including across journal replay.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum McpCallFailure {
    UnknownTool {
        name: String,
    },
    UnknownToolId {
        tool_id: String,
    },
    InvalidArguments {
        tool: String,
        arguments: Value,
    },
    InvalidExecutionBinding {
        tool_id: String,
        binding: Value,
    },
    ExecutionBindingChanged {
        server: String,
        tool_id: String,
    },
    UnsupportedRemoteCompletion {
        server: String,
        tool_id: String,
    },
    PoolShutDown,
    ServerUnavailable {
        server: String,
        health: McpServerHealth,
        after_ms: u64,
    },
    ConnectionLost {
        server: String,
        cause: McpServiceFailure,
        after_ms: u64,
        shutting_down: bool,
    },
    CallTimeout {
        server: String,
        timeout_ms: u64,
        deadline: bool,
    },
    JsonRpc {
        error: ErrorData,
    },
    UnexpectedResponse,
    UnsupportedSdkError {
        diagnostic: String,
    },
    AttachmentDecode {
        cause: McpDecodeFailure,
    },
    AttachmentMime {
        media_type: String,
    },
    AttachmentStore {
        cause: AttachmentRetentionFailure,
        diagnostic: String,
    },
    ToolError {
        message: String,
        content: ToolValue,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum McpServiceFailure {
    TransportClosed,
    TransportSend { diagnostic: String },
    UnexpectedResponse,
    JsonRpc { error: ErrorData },
    Timeout { timeout_ms: u64 },
    Cancelled { reason: Option<String> },
    ConsecutiveTimeouts { count: u64 },
    UnsupportedSdkError { diagnostic: String },
}

impl From<ServiceError> for McpServiceFailure {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::TransportClosed => Self::TransportClosed,
            ServiceError::TransportSend(error) => Self::TransportSend {
                diagnostic: error.to_string(),
            },
            ServiceError::UnexpectedResponse => Self::UnexpectedResponse,
            ServiceError::McpError(error) => Self::JsonRpc { error },
            ServiceError::Timeout { timeout } => Self::Timeout {
                timeout_ms: timeout.as_millis() as u64,
            },
            ServiceError::Cancelled { reason } => Self::Cancelled { reason },
            // The pinned SDK is non-exhaustive. An extension is recorded as an
            // explicit unsupported cause, never classified as a transport loss.
            error => Self::UnsupportedSdkError {
                diagnostic: error.to_string(),
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum McpDecodeFailure {
    Byte { offset: usize, byte: u8 },
    Length { length: usize },
    LastSymbol { offset: usize, byte: u8 },
    Padding,
}

impl From<base64::DecodeError> for McpDecodeFailure {
    fn from(error: base64::DecodeError) -> Self {
        match error {
            base64::DecodeError::InvalidByte(offset, byte) => Self::Byte { offset, byte },
            base64::DecodeError::InvalidLength(length) => Self::Length { length },
            base64::DecodeError::InvalidLastSymbol { offset, symbol, .. } => Self::LastSymbol {
                offset,
                byte: symbol,
            },
            base64::DecodeError::InvalidPadding => Self::Padding,
        }
    }
}

impl std::fmt::Display for McpCallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool { name } => write!(f, "Unknown MCP tool: {name}"),
            Self::UnknownToolId { tool_id } => write!(f, "Unknown MCP tool id: {tool_id}"),
            Self::InvalidArguments { tool, arguments } => write!(
                f,
                "MCP tool `{tool}` expected an object argument, got {arguments}"
            ),
            Self::InvalidExecutionBinding { tool_id, .. } => write!(
                f,
                "MCP tool id `{tool_id}` requires an execution binding for its admitted server and tool"
            ),
            Self::ExecutionBindingChanged { server, tool_id } => write!(
                f,
                "MCP admitted binding for `{server}` tool `{tool_id}` is unavailable or changed"
            ),
            Self::UnsupportedRemoteCompletion { server, tool_id } => write!(
                f,
                "MCP tool `{tool_id}` on `{server}` requires a durable remote completion adapter; a socket supports inline attempts only"
            ),
            Self::PoolShutDown => f.write_str("MCP connection pool has shut down"),
            Self::ServerUnavailable { server, health, .. } => match health {
                McpServerHealth::Exhausted {
                    attempts,
                    last_error,
                } => write!(
                    f,
                    "MCP server `{server}` reconnect attempts exhausted after {attempts} attempt(s); no background recovery is active; last error: {}",
                    last_error
                        .as_ref()
                        .map(super::pool::McpServerFault::message)
                        .unwrap_or_else(|| "unknown connection error".into())
                ),
                McpServerHealth::ShuttingDown { .. } => write!(
                    f,
                    "MCP server `{server}` is unavailable because its pool entry is shutting down"
                ),
                McpServerHealth::Connecting
                | McpServerHealth::Connected { .. }
                | McpServerHealth::Reconnecting { .. } => {
                    write!(
                        f,
                        "MCP server `{server}` was disconnected before tool dispatch (reconnecting in the background"
                    )?;
                    if let Some(error) = health.error() {
                        write!(f, "; last error: {error}")?;
                    }
                    f.write_str(")")
                }
            },
            Self::ConnectionLost {
                server,
                cause,
                shutting_down,
                ..
            } => write!(
                f,
                "MCP server `{server}` connection lost: {cause:?}; {}",
                if *shutting_down {
                    "pool entry is shutting down"
                } else {
                    "reconnecting in the background"
                }
            ),
            Self::CallTimeout {
                server, timeout_ms, ..
            } => write!(
                f,
                "MCP tool call timed out for `{server}` after {timeout_ms}ms"
            ),
            Self::JsonRpc { error } => write!(f, "MCP protocol error: {error}"),
            Self::UnexpectedResponse => f.write_str("MCP protocol error: Unexpected response type"),
            Self::UnsupportedSdkError { diagnostic } => {
                write!(f, "Unsupported MCP SDK error: {diagnostic}")
            }
            Self::AttachmentDecode { cause } => {
                write!(f, "failed to decode MCP attachment payload: {cause:?}")
            }
            Self::AttachmentMime { media_type } => {
                write!(f, "Invalid MCP attachment MIME: `{media_type}`")
            }
            Self::AttachmentStore { diagnostic, .. } => {
                write!(f, "Failed to store MCP attachment: {diagnostic}")
            }
            Self::ToolError { message, .. } => f.write_str(message),
        }
    }
}

impl From<McpCallFailure> for ToolFailure {
    fn from(cause: McpCallFailure) -> Self {
        use McpCallFailure as F;
        use ToolFailureClass as C;
        use ToolFailureSource as S;
        let (class, code, source, suggested_delay_ms) = match &cause {
            F::UnknownTool { .. } => (C::InvalidRequest, "mcp_unknown_tool", S::Plugin, None),
            F::UnknownToolId { .. } => (C::InvalidRequest, "mcp_unknown_tool_id", S::Plugin, None),
            F::InvalidArguments { .. } => {
                (C::InvalidRequest, "mcp_invalid_arguments", S::Plugin, None)
            }
            F::InvalidExecutionBinding { .. } => (
                C::InvalidRequest,
                "mcp_invalid_execution_binding",
                S::Plugin,
                None,
            ),
            F::ExecutionBindingChanged { .. } => (
                C::Unavailable,
                "mcp_execution_binding_changed",
                S::Plugin,
                None,
            ),
            F::UnsupportedRemoteCompletion { .. } => (
                C::Unavailable,
                "mcp_unsupported_remote_completion",
                S::Plugin,
                None,
            ),
            F::PoolShutDown => (C::Unavailable, "mcp_pool_shut_down", S::Plugin, None),
            F::ServerUnavailable {
                health, after_ms, ..
            } => {
                let (code, suggested_delay_ms) = match health {
                    McpServerHealth::Exhausted { .. } => ("mcp_reconnect_exhausted", None),
                    McpServerHealth::ShuttingDown { .. } => ("mcp_server_unavailable", None),
                    McpServerHealth::Connecting
                    | McpServerHealth::Connected { .. }
                    | McpServerHealth::Reconnecting { .. } => {
                        ("mcp_server_unavailable", Some(*after_ms))
                    }
                };
                (C::Unavailable, code, S::Plugin, suggested_delay_ms)
            }
            F::ConnectionLost {
                after_ms,
                shutting_down,
                ..
            } => (
                C::Unavailable,
                if *shutting_down {
                    "mcp_server_unavailable"
                } else {
                    "mcp_connection_lost"
                },
                S::Plugin,
                if *shutting_down {
                    None
                } else {
                    Some(*after_ms)
                },
            ),
            F::CallTimeout { deadline, .. } => (
                C::Timeout,
                if *deadline {
                    "mcp_call_deadline_exceeded"
                } else {
                    "mcp_call_timeout"
                },
                S::Plugin,
                None,
            ),
            F::JsonRpc { error } => (
                if matches!(error.code.0, -32600 | -32601 | -32602 | -32700) {
                    C::InvalidRequest
                } else {
                    C::External
                },
                "mcp_json_rpc_error",
                S::Tool,
                None,
            ),
            F::UnexpectedResponse => (C::External, "mcp_unexpected_response", S::Plugin, None),
            F::UnsupportedSdkError { .. } => {
                (C::Internal, "mcp_unsupported_sdk_error", S::Plugin, None)
            }
            F::AttachmentDecode { .. } => (C::External, "mcp_attachment_decode", S::Plugin, None),
            F::AttachmentMime { .. } => (C::External, "mcp_attachment_mime", S::Plugin, None),
            // Retention happens after the external tool executed. Its transient
            // class never authorizes executing the tool a second time.
            F::AttachmentStore { cause, .. } => (
                cause.tool_failure_class(),
                "mcp_attachment_store",
                S::Plugin,
                None,
            ),
            F::ToolError { .. } => (C::Execution, "mcp_tool_error", S::Tool, None),
        };
        let schema_cause = if let F::ServerUnavailable { health, .. } = &cause {
            if let Some(super::pool::McpServerFault::UnusableSchema(source)) = health.fault() {
                Some(Box::new(lash_core::ToolFailureCause::ToolSchemaAdmission {
                    source: source.clone(),
                }))
            } else {
                None
            }
        } else {
            None
        };
        let message = cause.to_string();
        let raw = match cause {
            F::ToolError { message, content } => ToolValue::Object(BTreeMap::from([
                ("kind".into(), ToolValue::String("tool_error".into())),
                ("message".into(), ToolValue::String(message)),
                ("content".into(), content),
            ])),
            cause => ToolValue::untrusted_json(json!(cause)),
        };
        Self {
            cause: schema_cause,
            class,
            code: code.into(),
            message,
            source,
            suggested_delay_ms,
            raw: Some(raw),
        }
    }
}

impl From<McpCallFailure> for ToolOutcome {
    fn from(cause: McpCallFailure) -> Self {
        Self::failure(cause.into())
    }
}
