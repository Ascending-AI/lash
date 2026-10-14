//! MCP event subscriptions as host code, on the `lash` facade alone
//! (FIG-6178).
//!
//! A host enables one server's events with [`ticket_events`], binds the MCP
//! plugin's event handle to its core, and subscribes a session with
//! [`watch_tickets`]. From then on every `ticket.created` the server
//! publishes starts one turn in that session, exactly once, whatever
//! crashes, node handovers or redeliveries happen in between. [`report`]
//! reads each subscription's status without running it.

use lash::mcp::events::{
    McpDeliveryMode, McpEvents, McpEventsConfig, McpEventsError, McpReactionProfile,
    McpReactionTarget, McpSubscribeRequest,
};
use lash::{ProcessId, SessionId};

/// The event type this host reacts to.
pub const TICKET_CREATED: &str = "ticket.created";

/// The host's policy for its ticketing server: push when the server offers
/// it, else poll; a model may subscribe only its own session.
pub fn ticket_events() -> McpEventsConfig {
    McpEventsConfig::new([McpDeliveryMode::Push, McpDeliveryMode::Poll]).with_reactions(
        McpReactionProfile {
            own_session: true,
            ..McpReactionProfile::default()
        },
    )
}

/// Subscribe `session` to new tickets on `server`: each one becomes a turn
/// of `session`. The key is the host's: asking again answers the same
/// subscription.
///
/// # Errors
///
/// The server does not offer the event, or the core refused the start.
pub async fn watch_tickets(
    events: &McpEvents,
    server: &str,
    session: &SessionId,
) -> Result<ProcessId, McpEventsError> {
    events
        .subscribe(
            McpSubscribeRequest::host(
                server,
                TICKET_CREATED,
                serde_json::Map::new(),
                McpReactionTarget::Session {
                    session: session.clone(),
                },
                format!("watch-tickets:{server}:{session}"),
            )
            .with_observers([session.clone()]),
        )
        .await
}

/// One line per live subscription `session` observes: what it delivered,
/// and whether it may have missed events or is parked.
///
/// # Errors
///
/// The core refused the read.
pub async fn report(
    events: &McpEvents,
    session: &SessionId,
) -> Result<Vec<String>, McpEventsError> {
    Ok(events
        .list(Some(session))
        .await?
        .into_iter()
        .map(|subscription| match subscription.status {
            None => format!("{}: starting", subscription.id),
            Some(status) => format!(
                "{}: {} on {} by {:?}, {} delivered{}{}",
                subscription.id,
                status.event,
                status.server,
                status.mode,
                status.delivered,
                if status.degraded() { ", degraded" } else { "" },
                match &status.parked {
                    Some(reason) => format!(", parked: {reason:?}"),
                    None => String::new(),
                },
            ),
        })
        .collect())
}

#[cfg(test)]
mod tests;
