//! The terminal tombstone every admitted ingress item leaves (ADR 0101 §8).
//!
//! An input and a session command end the same way: the row
//! stays, with no admission binding, its submitted delivery and digest
//! unchanged, and a closed [`IngressTerminalCause`] with the instant it was
//! written. Open-row selection never takes a tombstone, a resubmission under
//! its source key and digest answers it, and only host vacuum removes it.

use serde::{Deserialize, Serialize};

/// Why an ingress item left the open queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IngressTerminalCause {
    /// A run delivered the input or wake into a turn it committed.
    Delivered,
    /// The command lane applied the session command.
    Applied,
    /// The host withdrew the item, or a cancellation dropped it, before any
    /// turn consumed it.
    Cancelled,
    /// A config command could not apply because its base revision was stale.
    StaleConfigRevision,
}

impl IngressTerminalCause {
    /// Every cause, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::Delivered,
        Self::Applied,
        Self::Cancelled,
        Self::StaleConfigRevision,
    ];

    /// The persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Applied => "applied",
            Self::Cancelled => "cancelled",
            Self::StaleConfigRevision => "stale_config_revision",
        }
    }

    /// The cause spelled `value`, `None` for any other spelling.
    pub fn from_wire_str(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|cause| cause.as_str() == value)
    }
}

/// An ingress item's terminal tombstone: its cause and the store-clock
/// instant the terminal write recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IngressTerminal {
    pub cause: IngressTerminalCause,
    pub at_ms: u64,
}
