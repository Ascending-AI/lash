//! The per-server opt-in to MCP events. A server whose `events` is
//! [`McpServerEvents::Disabled`], the default, contributes nothing new.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::McpError;

const DEFAULT_POLL_FLOOR_MS: u64 = 1_000;
const DEFAULT_MAX_EVENTS: u32 = 50;
const DEFAULT_PUSH_WINDOW_MS: u64 = 25_000;
const DEFAULT_FETCH_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_RETRY_INITIAL_MS: u64 = 1_000;
const DEFAULT_RETRY_MAX_MS: u64 = 60_000;
const DEFAULT_FAILURES_BEFORE_PARK: u32 = 5;
const DEFAULT_DISPATCH_TIMEOUT_MS: u64 = 10_000;

/// Whether a server's events are offered, and how. Host JSON is
/// `"disabled"` or `{"enabled": {...}}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerEvents {
    /// The server's events are not offered: no tool, no subscription.
    #[default]
    Disabled,
    /// The server's events are offered under this policy.
    Enabled(McpEventsConfig),
}

impl McpServerEvents {
    /// Whether events are off.
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }

    /// The policy, when events are on.
    pub fn enabled(&self) -> Option<&McpEventsConfig> {
        match self {
            Self::Disabled => None,
            Self::Enabled(config) => Some(config),
        }
    }
}

/// A SEP-3415 delivery mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpDeliveryMode {
    /// `events/poll`, at the server's `nextPollMs`.
    Poll,
    /// `events/stream`, one held request per subscription.
    Push,
}

impl McpDeliveryMode {
    /// The mode's wire name in an event descriptor's `delivery`.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Poll => "poll",
            Self::Push => "push",
        }
    }
}

/// How one server's events are offered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEventsConfig {
    /// The delivery modes the host lets a subscription use, in preference
    /// order. A subscription takes the first one its event type offers.
    pub modes: Vec<McpDeliveryMode>,
    /// The reactions a model's `subscribe_event` may choose.
    #[serde(default)]
    pub reactions: McpReactionProfile,
    /// The subscription's bounds.
    #[serde(default)]
    pub bounds: McpEventBounds,
}

impl McpEventsConfig {
    /// Events delivered by `modes`, in that preference, with no model
    /// reaction allowed and the standard bounds.
    pub fn new(modes: impl IntoIterator<Item = McpDeliveryMode>) -> Self {
        Self {
            modes: modes.into_iter().collect(),
            reactions: McpReactionProfile::default(),
            bounds: McpEventBounds::default(),
        }
    }

    /// The reactions a model may choose.
    #[must_use]
    pub fn with_reactions(mut self, reactions: McpReactionProfile) -> Self {
        self.reactions = reactions;
        self
    }

    /// The subscription's bounds.
    #[must_use]
    pub fn with_bounds(mut self, bounds: McpEventBounds) -> Self {
        self.bounds = bounds;
        self
    }

    pub(crate) fn validate(&self, server_name: &str) -> Result<(), McpError> {
        if self.modes.is_empty() {
            return Err(McpError::Config(format!(
                "MCP server `{server_name}` enables events with no delivery mode"
            )));
        }
        let distinct: BTreeSet<_> = self.modes.iter().collect();
        if distinct.len() != self.modes.len() {
            return Err(McpError::Config(format!(
                "MCP server `{server_name}` names an events delivery mode twice"
            )));
        }
        self.bounds.validate(server_name)
    }
}

/// What a model's `subscribe_event` may react with. Host subscriptions
/// ([`McpEvents::subscribe`](crate::events::McpEvents::subscribe)) are not
/// limited by it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpReactionProfile {
    /// Each event may start a keyed turn in the subscribing session.
    #[serde(default)]
    pub own_session: bool,
    /// Each event may run as a keyed turn of its own child session, built
    /// from the host's isolated reaction spec.
    #[serde(default)]
    pub isolated: bool,
    /// The host-registered workflow reactions, by name, a model may name.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub workflows: BTreeSet<String>,
}

/// A subscription's bounds. Body bounds stay short: the subscription's
/// process loop, not a long body, provides persistence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpEventBounds {
    /// The least wait between two polls, whatever `nextPollMs` says.
    pub poll_floor_ms: u64,
    /// The most events one poll or push window returns.
    pub max_events: u32,
    /// How long one push window holds its stream before it returns.
    pub push_window_ms: u64,
    /// The execution bound of one fetch (a poll, or a push window plus its
    /// close).
    pub fetch_timeout_ms: u64,
    /// The bound of one delivery of a dispatch.
    pub dispatch_timeout_ms: u64,
    /// SEP-3415 `maxAgeMs`: replay no older than this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_age_ms: Option<u64>,
    /// The first retry wait after a failed fetch, doubled per failure.
    pub retry_initial_ms: u64,
    /// The longest retry wait.
    pub retry_max_ms: u64,
    /// The failed fetches in a row after which the subscription parks.
    pub failures_before_park: u32,
}

impl Default for McpEventBounds {
    fn default() -> Self {
        Self {
            poll_floor_ms: DEFAULT_POLL_FLOOR_MS,
            max_events: DEFAULT_MAX_EVENTS,
            push_window_ms: DEFAULT_PUSH_WINDOW_MS,
            fetch_timeout_ms: DEFAULT_FETCH_TIMEOUT_MS,
            dispatch_timeout_ms: DEFAULT_DISPATCH_TIMEOUT_MS,
            max_age_ms: None,
            retry_initial_ms: DEFAULT_RETRY_INITIAL_MS,
            retry_max_ms: DEFAULT_RETRY_MAX_MS,
            failures_before_park: DEFAULT_FAILURES_BEFORE_PARK,
        }
    }
}

impl McpEventBounds {
    fn validate(&self, server_name: &str) -> Result<(), McpError> {
        let refuse = |what: &str| {
            Err(McpError::Config(format!(
                "MCP server `{server_name}` events bound {what}"
            )))
        };
        if self.max_events == 0 {
            return refuse("`max_events` must be at least one");
        }
        if self.push_window_ms == 0 || self.push_window_ms >= self.fetch_timeout_ms {
            return refuse("`push_window_ms` must be positive and below `fetch_timeout_ms`");
        }
        if self.fetch_timeout_ms == 0
            || Duration::from_millis(self.fetch_timeout_ms) >= super::steps::FETCH_EXECUTION
        {
            return refuse("`fetch_timeout_ms` must be positive and below the fetch step's 300 s");
        }
        // One dispatch delivers up to `max_events` items within its step.
        let dispatch = Duration::from_millis(self.dispatch_timeout_ms)
            .saturating_mul(self.max_events.saturating_add(1));
        if self.dispatch_timeout_ms == 0 || dispatch > super::steps::DISPATCH_EXECUTION {
            return refuse(
                "`dispatch_timeout_ms` must be positive, and `max_events + 1` deliveries of it \
                 must fit the dispatch step's 600 s",
            );
        }
        if self.retry_initial_ms == 0 || self.retry_max_ms < self.retry_initial_ms {
            return refuse("`retry_initial_ms` must be positive and at most `retry_max_ms`");
        }
        if self.failures_before_park == 0 {
            return refuse("`failures_before_park` must be at least one");
        }
        Ok(())
    }
}
