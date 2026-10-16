//! Per-server transport admission. Subscriptions and dispatch belong to hosts.
use serde::{Deserialize, Serialize};

/// Whether the host enables the experimental SEP-3415 transport on a server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerEvents {
    #[default]
    Disabled,
    Enabled(McpEventTransportPolicy),
}
impl McpServerEvents {
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }
    pub fn enabled(&self) -> Option<&McpEventTransportPolicy> {
        match self {
            Self::Disabled => None,
            Self::Enabled(policy) => Some(policy),
        }
    }
}

/// Host-selected transport bounds. Event listings share the server's
/// `McpDataLimits.catalog_pages`, `catalog_items` and `catalog_bytes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpEventTransportPolicy {
    /// Notifications retained before a request route is installed, and per
    /// open route. Default: 1,024. Overflow poisons connection continuity;
    /// reconnect before reopening, and reconcile from a committed cursor.
    pub orphan_notifications: usize,
    /// Time a window accepts reordered notifications after its closing
    /// heartbeat/response/deadline. Default: 50 ms; rmcp dispatches handlers
    /// independently. This is a latency policy, not a completeness guarantee.
    pub reorder_settle_ms: u64,
}
impl Default for McpEventTransportPolicy {
    fn default() -> Self {
        Self {
            orphan_notifications: 1024,
            reorder_settle_ms: 50,
        }
    }
}
