use lash_core::facade_support::ReconfigureError;

/// Errors from pool configuration, server startup, and registry reconfiguration.
/// Tool call failures carry their MCP cause in the returned tool outcome.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    #[error("{0}")]
    UnusableSchema(#[from] lash_vocabulary::ToolCatalogBuildError),
    #[error("MCP connection pool has shut down")]
    PoolShutDown,
    #[error("{0}")]
    Config(String),
    #[error("MCP transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid MCP JSON message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("MCP server `{server}` requires {expected}, but offered {offered:?}")]
    UnsupportedProtocolRevision {
        server: String,
        expected: String,
        offered: Vec<String>,
    },
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    #[error("MCP startup timed out for `{server}` after {timeout_ms}ms")]
    StartupTimeout {
        server: String,
        timeout_ms: u64,
        observation: Box<McpStartupObservation>,
    },
    #[error("{source}")]
    StartupFailure {
        #[source]
        source: Box<McpError>,
        observation: Box<McpStartupObservation>,
    },
    #[error("tool registration failed: {0}")]
    Reconfigure(#[from] ReconfigureError),
}

/// Configured peer identity excludes arguments, environment and HTTP secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpPeer {
    Stdio { command: String },
    StreamableHttp,
}

/// Exit evidence is observed, never inferred from a closed pipe or timeout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpExitObservation {
    Unavailable,
    NotObserved,
    BeforeFailure {
        code: Option<i32>,
        signal: Option<i32>,
    },
    DuringCleanup {
        code: Option<i32>,
        signal: Option<i32>,
        /// Cleanup requested termination; this does not claim which signal
        /// caused the observed exit.
        forced: bool,
    },
    ObservationFailed {
        kind: std::io::ErrorKind,
    },
}

/// Raw bytes enabled by host policy. Lash makes no redaction claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpDiagnosticContent {
    Omitted,
    Unavailable,
    Captured {
        bytes: Vec<u8>,
        truncated: bool,
        complete: bool,
    },
}

/// Decode classification and position carry no peer-controlled message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpDecodeCause {
    Json {
        category: serde_json::error::Category,
        line: usize,
        column: usize,
    },
    Io {
        kind: std::io::ErrorKind,
    },
    LineTooLong,
    Other,
}

/// Latest recoverable decode observation; retained across later valid frames.
#[derive(Clone, Debug)]
pub struct McpInvalidLine {
    pub cause: McpDecodeCause,
    pub prefix: McpDiagnosticContent,
}

/// The last failed startup reported by the pool, until a connection succeeds.
#[derive(Clone, Debug)]
pub struct McpStartupFailure {
    pub message: String,
    pub observation: McpStartupObservation,
    pub protocol_refusal: Option<McpProtocolRevisionRefusal>,
}

/// Typed revision mismatch retained when an unavailable server stays configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpProtocolRevisionRefusal {
    /// Always [`MCP_PROTOCOL_VERSION`](crate::MCP_PROTOCOL_VERSION).
    pub expected: String,
    /// The revisions the server offered instead.
    pub offered: Vec<String>,
}

/// Evidence from one startup attempt, retained after its bounded cleanup.
#[derive(Clone, Debug)]
pub struct McpStartupObservation {
    pub peer: McpPeer,
    /// Runtime-clock deadline of the phase that failed (discover or discovery).
    pub deadline: tokio::time::Instant,
    pub timeout_ms: u64,
    pub exit: McpExitObservation,
    pub stderr: McpDiagnosticContent,
    pub invalid_line: Option<McpInvalidLine>,
}

impl McpError {
    /// Typed startup evidence survives cleanup independently of display text.
    pub fn startup_observation(&self) -> Option<&McpStartupObservation> {
        match self {
            Self::StartupFailure { observation, .. } | Self::StartupTimeout { observation, .. } => {
                Some(observation)
            }
            _ => None,
        }
    }

    pub(crate) fn with_startup_observation(self, observation: Box<McpStartupObservation>) -> Self {
        match self {
            Self::StartupTimeout {
                server, timeout_ms, ..
            } => Self::StartupTimeout {
                server,
                timeout_ms,
                observation,
            },
            source => Self::StartupFailure {
                source: Box::new(source),
                observation,
            },
        }
    }
}
