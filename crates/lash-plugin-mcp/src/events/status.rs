//! A subscription's typed facts: what its engine state records and what its
//! status projection shows a host, read back from the last committed
//! transition ([`McpEvents::status`](super::McpEvents::status)).

use serde::{Deserialize, Serialize};

use super::client::McpEventsCallError;
use super::config::McpDeliveryMode;
use super::spec::McpMappingError;
use super::wire::McpEventsWireError;

/// How many of each kind of fact the status keeps, newest last; the counts
/// cover all of them.
pub(crate) const RECENT_FACTS: usize = 16;

/// A gap: the server resumed later than the cursor it was given
/// (`truncated`), so events may have been skipped. Lash never invents
/// completeness: the gap stays on record and reaches the reaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEventGap {
    /// The gap's ordinal on the subscription, from one.
    pub ordinal: u64,
    /// When the fetch that reported it ran, in store milliseconds.
    pub observed_at_ms: i64,
    /// Whether the subscription held a cursor the server could not honour.
    pub had_cursor: bool,
}

/// Why one delivery started nothing. The occurrence is not delivered again
/// by this subscription; the failure stays on record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpDeliveryError {
    /// The event did not map to the workflow's arguments.
    #[error("the event did not map to start arguments: {0}")]
    Mapping(McpMappingError),
    /// The mapped arguments do not fit the definition's signature.
    #[error("the mapped arguments do not fit the definition (`{code}`): {message}")]
    Arguments { code: String, message: String },
    /// The derived key already names different content: the server sent a
    /// different occurrence under an `eventId` it delivered before.
    #[error("the derived key names different content: {message}")]
    Conflict { message: String },
    /// The reaction target refused it for good, such as a deleted session.
    #[error("the reaction refused it: {message}")]
    Refused { message: String },
}

/// One recorded delivery failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpDeliveryFailure {
    /// What the delivery delivered: `event:<eventId>` or `gap:<n>`.
    pub item: String,
    pub error: McpDeliveryError,
}

/// Why a subscription is parked: it waits on its resume key until a host
/// resumes it ([`McpEvents::resume`](super::McpEvents::resume)).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpParkReason {
    /// The server refused the principal (`Forbidden`), at subscribe or by
    /// ending the subscription.
    Forbidden { error: McpEventsWireError },
    /// The server stayed saturated (`ResourceExhausted`) for
    /// `failures` fetches in a row.
    Saturated {
        failures: u32,
        last: McpEventsCallError,
    },
    /// The server stayed unreachable or unanswering for `failures` fetches
    /// in a row.
    Unavailable {
        failures: u32,
        last: McpEventsCallError,
    },
}

/// Why a subscription ended on its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpSubscriptionEnd {
    /// The server refused the subscription for good (an unknown event, a
    /// schema change, arguments it does not admit, or no events extension).
    #[error("the server ended the subscription: {error}")]
    Refused { error: McpEventsCallError },
    /// The server terminated the push stream.
    #[error("the server terminated the subscription: {} ({})", error.message, error.code)]
    Terminated { error: McpEventsWireError },
}

/// What a host reads of a subscription: the projection of its committed
/// engine state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSubscriptionStatus {
    pub server: String,
    pub event: String,
    pub mode: McpDeliveryMode,
    /// Whether the subscription holds a resume position.
    pub has_cursor: bool,
    /// Deliveries made, counting an occurrence the server handed over again
    /// whose key found it already delivered: at least the occurrences
    /// delivered, never a second reaction.
    pub delivered: u64,
    /// All gaps, counted.
    pub gaps: u64,
    /// The latest gaps.
    pub recent_gaps: Vec<McpEventGap>,
    /// All delivery failures, counted.
    pub delivery_failures: u64,
    /// The latest delivery failures.
    pub recent_delivery_failures: Vec<McpDeliveryFailure>,
    /// Failed fetches in a row, and the last one's cause.
    pub fetch_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fetch_failure: Option<McpEventsCallError>,
    /// Why it is parked, while it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<McpParkReason>,
    /// Why it ended, once the server ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<McpSubscriptionEnd>,
}

impl McpSubscriptionStatus {
    /// Whether the subscription saw a gap or a failed delivery: it may not
    /// have reacted to everything the server produced.
    pub fn degraded(&self) -> bool {
        self.gaps > 0 || self.delivery_failures > 0
    }
}

/// Push `fact` onto `recent`, keeping the newest [`RECENT_FACTS`].
pub(crate) fn remember<T>(recent: &mut Vec<T>, fact: T) {
    recent.push(fact);
    if recent.len() > RECENT_FACTS {
        recent.remove(0);
    }
}
