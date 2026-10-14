//! The `events/*` requests on a pooled connection, and the routing of
//! `notifications/events/*` to the stream that asked for them.
//!
//! rmcp hands every notification to the client handler on a task of its own,
//! so two notifications of one stream can reach it in either order. A push
//! window therefore closes on a heartbeat, keeps reading for a short settle
//! after it, and persists the heartbeat's cursor (the position the server
//! checked up to). An event that arrives later than the settle is not lost:
//! the next window reopens at that cursor, and if the event lies past it the
//! server delivers it again, where its derived key de-duplicates it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt as _;
use rmcp::ServiceError;
use rmcp::model::{ClientRequest, CustomRequest, ServerResult};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient};
use serde_json::Value;
use tokio::sync::mpsc;

use super::config::McpServerEvents;
use super::wire::{
    self, EVENTS_EXTENSION, ListPage, McpEventDescriptor, McpEventOccurrence, McpEventsRefusal,
    McpEventsWireError, PollResult, StreamPosition,
};
use crate::pool::McpConnectionPool;

/// The most `events/list` pages one listing reads.
const MAX_LIST_PAGES: usize = 64;
/// How long a push window keeps reading after the notification that closed
/// it, for notifications rmcp dispatched before it and delivered after.
const PUSH_SETTLE: Duration = Duration::from_millis(50);
/// The most stream notifications held for a subscription id that has no
/// route yet: those that beat the stream's own registration.
const MAX_ORPHANS: usize = 1024;

/// Why an events request did not answer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpEventsCallError {
    /// The pool holds no server of this name.
    #[error("no MCP server `{server}` is configured")]
    UnknownServer { server: String },
    /// The server's events are not enabled by the host.
    #[error("MCP server `{server}` does not offer events: the host did not enable them")]
    Disabled { server: String },
    /// The server is not connected now.
    #[error("MCP server `{server}` is not connected")]
    Disconnected { server: String },
    /// The connected server does not advertise the events extension.
    #[error("MCP server `{server}` does not advertise `{EVENTS_EXTENSION}`")]
    NotAdvertised { server: String },
    /// The server answered with an error.
    #[error("MCP server `{server}` refused `{method}`: {} ({})", error.message, error.code)]
    Refused {
        server: String,
        method: String,
        refusal: McpEventsRefusal,
        error: McpEventsWireError,
    },
    /// The request did not answer within its bound.
    #[error("MCP server `{server}` did not answer `{method}` within {timeout_ms} ms")]
    Timeout {
        server: String,
        method: String,
        timeout_ms: u64,
    },
    /// The transport failed.
    #[error("MCP server `{server}` transport failed during `{method}`: {message}")]
    Transport {
        server: String,
        method: String,
        message: String,
    },
    /// The answer does not decode as the extension defines it.
    #[error("MCP server `{server}` answered `{method}` with an undecodable result: {message}")]
    Decode {
        server: String,
        method: String,
        message: String,
    },
}

impl McpEventsCallError {
    /// The server's refusal class, when the server refused.
    pub fn refusal(&self) -> Option<McpEventsRefusal> {
        match self {
            Self::Refused { refusal, .. } => Some(*refusal),
            _ => None,
        }
    }
}

/// What one push window read.
#[derive(Clone, Debug, Default)]
pub(crate) struct StreamWindow {
    pub(crate) events: Vec<McpEventOccurrence>,
    /// The position to reopen at: the closing heartbeat's, else the last
    /// position the stream named.
    pub(crate) cursor: Option<String>,
    /// Whether the stream reported a gap (`active` with `truncated`).
    pub(crate) truncated: bool,
    /// The server ended the subscription (`terminated`).
    pub(crate) terminated: Option<McpEventsWireError>,
    /// Transient per-occurrence errors the server reported.
    pub(crate) errors: Vec<McpEventsWireError>,
}

/// The routes of one connection's open streams, by their request id.
#[derive(Default)]
pub(crate) struct EventStreams {
    inner: Mutex<StreamRoutes>,
}

#[derive(Default)]
struct StreamRoutes {
    routes: BTreeMap<String, mpsc::UnboundedSender<(String, Value)>>,
    orphans: VecDeque<(String, String, Value)>,
}

impl EventStreams {
    /// Route a `notifications/events/*` notification of `subscription`.
    pub(crate) fn deliver(&self, subscription: &Value, method: String, params: Value) {
        let key = subscription.to_string();
        let mut inner = self.inner.lock_recover();
        if let Some(route) = inner.routes.get(&key) {
            let _ = route.send((method, params));
            return;
        }
        if inner.orphans.len() == MAX_ORPHANS {
            inner.orphans.pop_front();
        }
        inner.orphans.push_back((key, method, params));
    }

    fn open(&self, subscription: &Value) -> mpsc::UnboundedReceiver<(String, Value)> {
        let key = subscription.to_string();
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock_recover();
        let orphans = std::mem::take(&mut inner.orphans);
        for (orphan, method, params) in orphans {
            if orphan == key {
                let _ = sender.send((method, params));
            } else {
                inner.orphans.push_back((orphan, method, params));
            }
        }
        inner.routes.insert(key, sender);
        receiver
    }

    fn close(&self, subscription: &Value) {
        let key = subscription.to_string();
        let mut inner = self.inner.lock_recover();
        inner.routes.remove(&key);
        inner.orphans.retain(|(orphan, _, _)| *orphan != key);
    }
}

/// A connected peer for `server`'s events.
fn events_peer(
    pool: &McpConnectionPool,
    server: &str,
) -> Result<(Peer<RoleClient>, crate::host::LashMcpClientHandler), McpEventsCallError> {
    let (events, service) =
        pool.events_connection(server)
            .ok_or_else(|| McpEventsCallError::UnknownServer {
                server: server.to_owned(),
            })?;
    if matches!(events, McpServerEvents::Disabled) {
        return Err(McpEventsCallError::Disabled {
            server: server.to_owned(),
        });
    }
    let (peer, handler) = service.ok_or_else(|| McpEventsCallError::Disconnected {
        server: server.to_owned(),
    })?;
    let advertised = peer.peer_info().is_some_and(|info| {
        info.capabilities
            .extensions
            .as_ref()
            .is_some_and(|extensions| extensions.contains_key(EVENTS_EXTENSION))
    });
    if !advertised {
        return Err(McpEventsCallError::NotAdvertised {
            server: server.to_owned(),
        });
    }
    Ok((peer, handler))
}

fn request(method: &str, params: Value) -> ClientRequest {
    ClientRequest::CustomRequest(CustomRequest::new(method, Some(params)))
}

fn service_error(
    server: &str,
    method: &str,
    timeout: Duration,
    error: ServiceError,
) -> McpEventsCallError {
    match error {
        ServiceError::McpError(error) => McpEventsCallError::Refused {
            server: server.to_owned(),
            method: method.to_owned(),
            refusal: McpEventsRefusal::of_code(error.code.0),
            error: McpEventsWireError {
                code: error.code.0,
                message: error.message.to_string(),
                data: error.data,
            },
        },
        ServiceError::Timeout { .. } => McpEventsCallError::Timeout {
            server: server.to_owned(),
            method: method.to_owned(),
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        },
        other => McpEventsCallError::Transport {
            server: server.to_owned(),
            method: method.to_owned(),
            message: other.to_string(),
        },
    }
}

fn result_value(result: ServerResult) -> Result<Value, String> {
    match result {
        ServerResult::CustomResult(result) => Ok(result.0),
        other => serde_json::to_value(other).map_err(|error| error.to_string()),
    }
}

async fn call(
    pool: &McpConnectionPool,
    server: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, McpEventsCallError> {
    let (peer, _) = events_peer(pool, server)?;
    let result = peer
        .send_request_with_option(
            request(method, params),
            PeerRequestOptions::with_timeout(timeout),
        )
        .await
        .map_err(|error| service_error(server, method, timeout, error))?
        .await_response()
        .await
        .map_err(|error| service_error(server, method, timeout, error))?;
    result_value(result).map_err(|message| McpEventsCallError::Decode {
        server: server.to_owned(),
        method: method.to_owned(),
        message,
    })
}

fn decode<T: serde::de::DeserializeOwned>(
    server: &str,
    method: &str,
    value: Value,
) -> Result<T, McpEventsCallError> {
    serde_json::from_value(value).map_err(|error| McpEventsCallError::Decode {
        server: server.to_owned(),
        method: method.to_owned(),
        message: error.to_string(),
    })
}

impl McpConnectionPool {
    /// The event types `server` lists, every page, bounded.
    pub async fn list_events(
        &self,
        server: &str,
        timeout: Duration,
    ) -> Result<Vec<McpEventDescriptor>, McpEventsCallError> {
        let mut events = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(cursor) => serde_json::json!({ "cursor": cursor }),
                None => serde_json::json!({}),
            };
            let page: ListPage = decode(
                server,
                wire::LIST_METHOD,
                call(self, server, wire::LIST_METHOD, params, timeout).await?,
            )?;
            events.extend(page.events);
            match page.next_cursor {
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => return Ok(events),
            }
        }
        Err(McpEventsCallError::Decode {
            server: server.to_owned(),
            method: wire::LIST_METHOD.to_owned(),
            message: format!("the listing ran past {MAX_LIST_PAGES} pages"),
        })
    }

    /// One `events/poll`.
    pub(crate) async fn poll_events(
        &self,
        server: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<PollResult, McpEventsCallError> {
        decode(
            server,
            wire::POLL_METHOD,
            call(self, server, wire::POLL_METHOD, params, timeout).await?,
        )
    }

    /// One push window: open `events/stream`, read until a heartbeat closes
    /// the window or `window` passes, settle, then cancel the stream (the
    /// subscription's remote cancel).
    pub(crate) async fn stream_events(
        &self,
        server: &str,
        params: Value,
        window: Duration,
        max_events: usize,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<StreamWindow, McpEventsCallError> {
        let method = wire::STREAM_METHOD;
        let (peer, handler) = events_peer(self, server)?;
        let handle = peer
            .send_request_with_option(request(method, params), PeerRequestOptions::no_options())
            .await
            .map_err(|error| service_error(server, method, window, error))?;
        let subscription = handle.id.clone().into_json_value();
        let streams = handler.event_streams();
        let mut notifications = streams.open(&subscription);
        let mut response = handle.rx;
        let mut out = StreamWindow::default();
        let deadline = tokio::time::Instant::now() + window;
        let mut closing: Option<tokio::time::Instant> = None;
        let mut answered = false;
        let mut failure = None;
        loop {
            let until = closing.unwrap_or(deadline);
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = tokio::time::sleep_until(until) => {
                    if closing.is_some() {
                        break;
                    }
                    closing = Some(tokio::time::Instant::now() + PUSH_SETTLE);
                }
                notification = notifications.recv() => {
                    let Some((method, params)) = notification else { break };
                    let close = apply(&mut out, &method, params).map_err(|message| {
                        McpEventsCallError::Decode {
                            server: server.to_owned(),
                            method: method.clone(),
                            message,
                        }
                    });
                    match close {
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                        Ok(close) if close || out.events.len() >= max_events => {
                            closing.get_or_insert_with(|| tokio::time::Instant::now() + PUSH_SETTLE);
                        }
                        Ok(_) => {}
                    }
                }
                result = &mut response, if !answered => {
                    answered = true;
                    match result {
                        // The server closed the stream: what it sent is the window.
                        Ok(Ok(_)) => {
                            closing.get_or_insert_with(|| tokio::time::Instant::now() + PUSH_SETTLE);
                        }
                        Ok(Err(error)) => {
                            failure = Some(service_error(server, method, window, error));
                            break;
                        }
                        Err(_) => {
                            failure = Some(McpEventsCallError::Transport {
                                server: server.to_owned(),
                                method: method.to_owned(),
                                message: "the stream closed".to_owned(),
                            });
                            break;
                        }
                    }
                }
            }
        }
        streams.close(&subscription);
        if !answered {
            // The remote cancel: stdio's `notifications/cancelled`, and the
            // request stream's end over HTTP.
            let _ = peer.send_notification(cancel_notification(handle.id)).await;
        }
        match failure {
            Some(error) if out.events.is_empty() => Err(error),
            _ => Ok(out),
        }
    }
}

fn cancel_notification(id: rmcp::model::RequestId) -> rmcp::model::ClientNotification {
    rmcp::model::CancelledNotification::new(rmcp::model::CancelledNotificationParam::new(
        Some(id),
        Some("the subscription's window closed".to_owned()),
    ))
    .into()
}

/// Fold one stream notification into `out`; whether it closes the window.
fn apply(out: &mut StreamWindow, method: &str, params: Value) -> Result<bool, String> {
    match method {
        wire::EVENT_NOTIFICATION => {
            let event: McpEventOccurrence =
                serde_json::from_value(params).map_err(|error| error.to_string())?;
            if event.cursor.is_some() {
                out.cursor.clone_from(&event.cursor);
            }
            out.events.push(event);
            Ok(false)
        }
        wire::ACTIVE_NOTIFICATION => {
            let position: StreamPosition =
                serde_json::from_value(params).map_err(|error| error.to_string())?;
            out.truncated |= position.truncated;
            if out.events.is_empty() || position.truncated {
                out.cursor = position.cursor;
            }
            Ok(false)
        }
        wire::HEARTBEAT_NOTIFICATION => {
            let position: StreamPosition =
                serde_json::from_value(params).map_err(|error| error.to_string())?;
            out.cursor = position.cursor;
            Ok(true)
        }
        wire::ERROR_NOTIFICATION => {
            out.errors.push(wire_error(&params)?);
            Ok(false)
        }
        wire::TERMINATED_NOTIFICATION => {
            out.terminated = Some(wire_error(&params)?);
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn wire_error(params: &Value) -> Result<McpEventsWireError, String> {
    params
        .get("error")
        .cloned()
        .ok_or_else(|| "the notification carries no error".to_owned())
        .and_then(|error| serde_json::from_value(error).map_err(|error| error.to_string()))
}

/// The handler's routing of a custom notification: `true` when it was an
/// events stream notification.
pub(crate) fn route_notification(
    streams: &Arc<EventStreams>,
    notification: &rmcp::model::CustomNotification,
    subscription: Option<Value>,
) -> bool {
    if !notification.method.starts_with("notifications/events/") {
        return false;
    }
    let params = notification.params.clone().unwrap_or(Value::Null);
    let Some(subscription) = subscription.or_else(|| wire::subscription_id(&params).cloned())
    else {
        return false;
    };
    streams.deliver(&subscription, notification.method.clone(), params);
    true
}
