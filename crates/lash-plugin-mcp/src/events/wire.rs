//! The SEP-3415 wire contract this client speaks: the extension's
//! identifier, event descriptors and occurrences, `events/poll` and the
//! `events/stream` notifications, and the extension's error codes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The extension's identifier in `capabilities.extensions`.
pub const EVENTS_EXTENSION: &str = "io.modelcontextprotocol/events";

pub(crate) const LIST_METHOD: &str = "events/list";
pub(crate) const POLL_METHOD: &str = "events/poll";
pub(crate) const STREAM_METHOD: &str = "events/stream";
pub(crate) const ACTIVE_NOTIFICATION: &str = "notifications/events/active";
pub(crate) const EVENT_NOTIFICATION: &str = "notifications/events/event";
pub(crate) const HEARTBEAT_NOTIFICATION: &str = "notifications/events/heartbeat";
pub(crate) const ERROR_NOTIFICATION: &str = "notifications/events/error";
pub(crate) const TERMINATED_NOTIFICATION: &str = "notifications/events/terminated";
pub(crate) const SUBSCRIPTION_ID_META: &str = "io.modelcontextprotocol/subscriptionId";

/// One event type a server lists.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpEventDescriptor {
    /// The event type's name.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The delivery modes the server offers, as wire names. A name this
    /// client does not speak is kept and never chosen.
    pub delivery: Vec<String>,
    /// The JSON Schema of a subscription's arguments.
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    /// The JSON Schema of an occurrence's `data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_schema: Option<Value>,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// One event occurrence. Its `data` is untrusted data from the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpEventOccurrence {
    /// The server's stable identifier, the de-duplication key.
    pub event_id: String,
    /// The event type's name.
    pub name: String,
    /// When the event occurred (ISO 8601).
    pub timestamp: String,
    /// The payload.
    pub data: Value,
    /// Push only: the position after this event. Absent means `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl McpEventOccurrence {
    /// The occurrence as a reaction reads it: everything but its cursor,
    /// which is delivery bookkeeping and never part of what it delivers.
    pub(crate) fn delivered(&self) -> Value {
        serde_json::json!({
            "eventId": self.event_id,
            "name": self.name,
            "timestamp": self.timestamp,
            "data": self.data,
        })
    }
}

/// An `events/list` page.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListPage {
    #[serde(default)]
    pub(crate) events: Vec<McpEventDescriptor>,
    #[serde(default)]
    pub(crate) next_cursor: Option<String>,
}

/// An `events/poll` result.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PollResult {
    #[serde(default)]
    pub(crate) events: Vec<McpEventOccurrence>,
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    #[serde(default)]
    pub(crate) truncated: bool,
    #[serde(default)]
    pub(crate) has_more: bool,
    #[serde(default)]
    pub(crate) next_poll_ms: Option<u64>,
}

/// The `active` and `heartbeat` notifications' cursor and gap.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamPosition {
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    #[serde(default)]
    pub(crate) truncated: bool,
}

/// A JSON-RPC error a server answered, or carried in a `terminated` or
/// `error` notification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpEventsWireError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The class of a server's refusal, from SEP-3415's error codes: what a
/// subscription does about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpEventsRefusal {
    /// `-32601`: the server does not speak the extension.
    MethodNotFound,
    /// `-32602`: the arguments do not fit the event's `inputSchema`.
    InvalidParams,
    /// `-32023`: the event type does not exist (or no longer does).
    NotFound,
    /// `-32024`: the principal may not subscribe, or its access ended.
    Forbidden,
    /// `-32025`: a server limit or quota was reached.
    ResourceExhausted,
    /// `-32026`: the request asks for something the server does not offer.
    Unsupported,
    /// Any other code.
    Other,
}

impl McpEventsRefusal {
    pub(crate) fn of_code(code: i32) -> Self {
        match code {
            -32601 => Self::MethodNotFound,
            -32602 => Self::InvalidParams,
            -32023 => Self::NotFound,
            -32024 => Self::Forbidden,
            -32025 => Self::ResourceExhausted,
            -32026 => Self::Unsupported,
            _ => Self::Other,
        }
    }
}

/// The `_meta` subscription id of a stream notification.
pub(crate) fn subscription_id(params: &Value) -> Option<&Value> {
    params.get("_meta")?.get(SUBSCRIPTION_ID_META)
}
