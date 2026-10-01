use lash_core::facade_support::ReconfigureError;

/// Errors from pool configuration, server startup, and registry reconfiguration.
/// Tool call failures carry their MCP cause in the returned tool outcome.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    #[error("{0}")]
    UnusableSchema(#[from] lash_core::ToolCatalogBuildError),
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
    #[error("tool registration failed: {0}")]
    Reconfigure(#[from] ReconfigureError),
}
