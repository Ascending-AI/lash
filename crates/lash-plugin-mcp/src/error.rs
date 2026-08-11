use lash_core::facade_support::ReconfigureError;

/// Errors surfaced by `lash-plugin-mcp` when a server fails to connect,
/// when a tool call errors out, or when the registry rejects the new
/// surface.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP connection pool has shut down")]
    PoolShutDown,
    #[error("{0}")]
    Config(String),
    #[error("MCP transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid MCP JSON message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    #[error("MCP startup timed out for `{server}` after {timeout_ms}ms")]
    StartupTimeout { server: String, timeout_ms: u64 },
    #[error("MCP tool call timed out for `{server}` after {timeout_ms}ms")]
    CallTimeout { server: String, timeout_ms: u64 },
    #[error("failed to decode MCP image payload: {0}")]
    Decode(#[from] base64::DecodeError),
    #[error("tool registration failed: {0}")]
    Reconfigure(#[from] ReconfigureError),
}
