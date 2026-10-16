//! Typed MCP event transport (SEP-3415). Hosts own subscription registries,
//! cursors, schedules, routing, mapping and keyed delivery into Lash.
//! Enabling transport registers no process engine or model tools.
pub(crate) mod client;
mod config;
#[cfg(feature = "testing")]
pub mod testing;
pub(crate) mod wire;

pub use client::{
    McpEventClient, McpEventListingLimit, McpEventStream, McpEventWindow, McpEventsCallError,
};
pub use config::{McpEventTransportPolicy, McpServerEvents};
pub use wire::{
    EVENTS_EXTENSION, McpEventContinuityLoss, McpEventDescriptor, McpEventOccurrence, McpEventPoll,
    McpEventRequest, McpEventsRefusal, McpEventsWireError,
};
