use serde::{Deserialize, Serialize};

/// The observed cause of a failed language node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceLanguageExecutionFailure {
    /// A dispatched tool effect failed with its recorded classification and
    /// recorded source and retry status. `replay_key` is the causing effect's identity.
    Effect {
        class: lash_sansio::ToolFailureClass,
        code: String,
        message: String,
        replay_key: String,
        source: lash_sansio::ToolFailureSource,
        retry: lash_sansio::ToolRetryStatus,
    },
    /// A VM or host-boundary failure with a stable runtime error code.
    Runtime { code: String, message: String },
}

impl TraceLanguageExecutionFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::Effect { message, .. } | Self::Runtime { message, .. } => message,
        }
    }
}
