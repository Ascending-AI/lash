//! MCP events (SEP-3415), an opt-in per server: each subscription is a
//! durable lash process of the plugin's subscription engine, and each event
//! it reads durably starts one reaction, once.
//!
//! A server whose [`McpServerEvents`] is `Disabled`, the default, changes
//! nothing: with no server enabled the plugin registers no engine and no
//! tool. With one enabled, a model subscribes through the event tools
//! within the server's [`McpReactionProfile`], and a host through
//! [`McpEvents`] after binding it to its core.
//!
//! Delivery is at least once from the server and exactly once into lash:
//! every reaction is keyed by (subscription, generation, `eventId`), so a
//! redelivery, a rerun or a recovery after any crash starts nothing new.
//! The server's guarantee holds only within its retention: a resume past it
//! (`truncated`) is recorded as a typed gap, never papered over.
//!
//! Webhook delivery is not offered: poll and push only.

pub(crate) mod client;
mod config;
pub(crate) mod engine;
mod runtime;
mod spec;
mod status;
mod steps;
#[cfg(feature = "testing")]
pub mod testing;
mod tools;
pub(crate) mod wire;

pub use client::McpEventsCallError;
pub use config::{
    McpDeliveryMode, McpEventBounds, McpEventsConfig, McpReactionProfile, McpServerEvents,
};
pub use engine::{MCP_EVENTS_ENGINE, MCP_EVENTS_RESUME_KEY};
pub use runtime::{
    McpEvents, McpEventsActivity, McpEventsCore, McpEventsError, McpEventsPortError,
    McpSubscribeRequest, McpSubscription,
};
pub use spec::{
    McpInputMapping, McpMappingError, McpReactionTarget, McpSubscriptionSpec, McpWorkflowReaction,
};
pub use status::{
    McpDeliveryError, McpDeliveryFailure, McpEventGap, McpParkReason, McpSubscriptionEnd,
    McpSubscriptionStatus,
};
pub use steps::MCP_EVENTS_SESSION_TURN;
pub use tools::{
    LIST_EVENTS_TOOL, LIST_SUBSCRIPTIONS_TOOL, SUBSCRIBE_EVENT_TOOL, UNSUBSCRIBE_TOOL,
};
pub use wire::{
    EVENTS_EXTENSION, McpEventDescriptor, McpEventOccurrence, McpEventsRefusal, McpEventsWireError,
};

/// The engine registration, with its steps and status projection, over
/// `events`.
pub(crate) fn registration(events: &McpEvents) -> lash_core::ProcessEngineRegistration {
    engine::registration(std::sync::Arc::new(steps::SubscriptionSteps {
        events: events.clone(),
    }))
}

/// The model's event tools over `events`.
pub(crate) fn tool_provider(events: &McpEvents) -> std::sync::Arc<dyn lash_core::ToolProvider> {
    std::sync::Arc::new(tools::provider(events.clone()))
}
