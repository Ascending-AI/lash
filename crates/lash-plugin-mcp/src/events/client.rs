//! Typed requests and bounded push windows. rmcp dispatches notifications
//! independently; the host selects a settle interval and owns cursor commits.
use super::wire::{
    self, EVENTS_EXTENSION, ListPage, McpEventContinuityLoss, McpEventDescriptor,
    McpEventOccurrence, McpEventPoll, McpEventRequest, McpEventsRefusal, McpEventsWireError,
    StreamPosition,
};
use super::{McpEventTransportPolicy, McpServerEvents};
use crate::config::McpDataLimits;
use crate::pool::McpConnectionPool;
use lash_protocol_types::sync::MutexExt as _;
use rmcp::ServiceError;
use rmcp::model::{ClientRequest, CustomRequest, ServerResult};
use rmcp::service::{Peer, PeerRequestOptions, RequestHandle, RoleClient};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Which shared catalog budget refused an event listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpEventListingLimit {
    Pages,
    Items,
    Bytes,
}
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
    /// Host admission refused a listing before accepting a partial catalog.
    #[error("MCP server `{server}` event listing exceeded {limit:?} ({bound})")]
    ListingLimit {
        server: String,
        limit: McpEventListingLimit,
        bound: usize,
    },
    /// The connection lost notifications; no window from it is complete.
    #[error("MCP server `{server}` event continuity was lost: {loss:?}")]
    ContinuityLost {
        server: String,
        loss: McpEventContinuityLoss,
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

/// What one push window read. A host commits its events before its cursor.
#[derive(Clone, Debug, Default)]
pub struct McpEventWindow {
    pub events: Vec<McpEventOccurrence>,
    pub cursor: Option<String>,
    pub truncated: bool,
    pub terminated: Option<McpEventsWireError>,
    pub errors: Vec<McpEventsWireError>,
}
impl McpEventWindow {
    pub fn continuity_loss(&self) -> Option<McpEventContinuityLoss> {
        self.truncated.then_some(McpEventContinuityLoss::Retention)
    }
}

pub(crate) struct EventStreams {
    capacity: usize,
    inner: Mutex<StreamRoutes>,
}
impl Default for EventStreams {
    fn default() -> Self {
        Self::new(McpEventTransportPolicy::default().orphan_notifications)
    }
}
#[derive(Default)]
struct StreamRoutes {
    routes: BTreeMap<String, mpsc::Sender<(String, Value)>>,
    orphans: VecDeque<(String, String, Value)>,
    loss: Option<McpEventContinuityLoss>,
}
impl EventStreams {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::default(),
        }
    }
    pub(crate) fn deliver(&self, subscription: &Value, method: String, params: Value) {
        let key = subscription.to_string();
        let mut inner = self.inner.lock_recover();
        if inner.loss.is_some() {
            return;
        }
        let loss = if let Some(route) = inner.routes.get(&key) {
            if route.try_send((method, params)).is_ok() {
                return;
            }
            McpEventContinuityLoss::NotificationOverflow {
                capacity: self.capacity,
            }
        } else if inner.orphans.len() < self.capacity {
            inner.orphans.push_back((key, method, params));
            return;
        } else {
            McpEventContinuityLoss::NotificationOverflow {
                capacity: self.capacity,
            }
        };
        // Loss is connection-wide: without retaining unbounded lost request
        // identities we cannot prove which future route is complete. Closing
        // senders wakes readers; reconnect constructs fresh routing state.
        inner.loss = Some(loss);
        inner.routes.clear();
        inner.orphans.clear();
    }
    fn loss(&self) -> Option<McpEventContinuityLoss> {
        self.inner.lock_recover().loss.clone()
    }
    fn open(
        &self,
        subscription: &Value,
    ) -> Result<mpsc::Receiver<(String, Value)>, McpEventContinuityLoss> {
        let key = subscription.to_string();
        let mut inner = self.inner.lock_recover();
        if let Some(loss) = &inner.loss {
            return Err(loss.clone());
        }
        let (sender, receiver) = mpsc::channel(self.capacity.max(1));
        let orphans = std::mem::take(&mut inner.orphans);
        for (orphan, method, params) in orphans {
            if orphan == key {
                let _ = sender.try_send((method, params));
            } else {
                inner.orphans.push_back((orphan, method, params));
            }
        }
        inner.routes.insert(key, sender);
        Ok(receiver)
    }
    fn close(&self, subscription: &Value) {
        let key = subscription.to_string();
        let mut inner = self.inner.lock_recover();
        inner.routes.remove(&key);
        inner.orphans.retain(|(orphan, _, _)| *orphan != key);
    }
}
/// One pool entry's enabled policy, data limits and live connection, admitted
/// together. An operation captures it once and runs on it to the end.
struct EventsConnection {
    policy: McpEventTransportPolicy,
    limits: McpDataLimits,
    peer: Peer<RoleClient>,
    handler: crate::host::LashMcpClientHandler,
}
fn admit(pool: &McpConnectionPool, server: &str) -> Result<EventsConnection, McpEventsCallError> {
    let (events, limits, service) =
        pool.events_connection(server)
            .ok_or_else(|| McpEventsCallError::UnknownServer {
                server: server.to_owned(),
            })?;
    let McpServerEvents::Enabled(policy) = events else {
        return Err(McpEventsCallError::Disabled {
            server: server.to_owned(),
        });
    };
    let (peer, handler) = service.ok_or_else(|| McpEventsCallError::Disconnected {
        server: server.to_owned(),
    })?;
    if !peer.peer_info().is_some_and(|info| {
        info.capabilities
            .extensions
            .as_ref()
            .is_some_and(|extensions| extensions.contains_key(EVENTS_EXTENSION))
    }) {
        return Err(McpEventsCallError::NotAdvertised {
            server: server.to_owned(),
        });
    }
    Ok(EventsConnection {
        policy,
        limits,
        peer,
        handler,
    })
}

/// Host-callable event transport over the plugin's shared connections.
///
/// Each operation runs on the connection it admitted. When that connection
/// closes, even because the server was replaced, the operation fails with its
/// typed transport error and never continues on the replacement; the host's
/// next operation admits the new connection.
#[derive(Clone)]
pub struct McpEventClient {
    pool: Arc<McpConnectionPool>,
    #[cfg(test)]
    between_pages: Option<Arc<tests::PageHook>>,
}
impl McpEventClient {
    pub fn new(pool: Arc<McpConnectionPool>) -> Self {
        Self {
            pool,
            #[cfg(test)]
            between_pages: None,
        }
    }

    #[cfg(test)]
    async fn pause_between_pages(&self) {
        if let Some(hook) = &self.between_pages {
            hook.reached.notify_one();
            hook.release.notified().await;
        }
    }

    #[cfg(not(test))]
    async fn pause_between_pages(&self) {}

    /// Discover all event descriptors within this server's shared catalog limits.
    pub async fn list(
        &self,
        server: &str,
        timeout: Duration,
    ) -> Result<Vec<McpEventDescriptor>, McpEventsCallError> {
        let connection = admit(&self.pool, server)?;
        let limits = connection.limits;
        let refuse = |limit, bound| McpEventsCallError::ListingLimit {
            server: server.to_owned(),
            limit,
            bound,
        };
        let mut events = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut bytes = 0usize;
        for _ in 0..limits.catalog_pages {
            let params = cursor.as_ref().map_or_else(
                || serde_json::json!({}),
                |c| serde_json::json!({"cursor":c}),
            );
            let value = call(&connection.peer, server, wire::LIST_METHOD, params, timeout).await?;
            bytes = bytes.saturating_add(value.to_string().len());
            if bytes > limits.catalog_bytes {
                return Err(refuse(McpEventListingLimit::Bytes, limits.catalog_bytes));
            }
            let page: ListPage = decode(server, wire::LIST_METHOD, value)?;
            if events.len().saturating_add(page.events.len()) > limits.catalog_items {
                return Err(refuse(McpEventListingLimit::Items, limits.catalog_items));
            }
            events.extend(page.events);
            match page.next_cursor {
                None => return Ok(events),
                Some(next) if seen.insert(next.clone()) => {
                    cursor = Some(next);
                    self.pause_between_pages().await;
                }
                Some(_) => {
                    return Err(McpEventsCallError::Decode {
                        server: server.to_owned(),
                        method: wire::LIST_METHOD.to_owned(),
                        message: "event listing cursor cycle".to_owned(),
                    });
                }
            }
        }
        Err(refuse(McpEventListingLimit::Pages, limits.catalog_pages))
    }
    /// One poll. The host owns scheduling, persistence and retention-gap handling.
    pub async fn poll(
        &self,
        server: &str,
        input: &McpEventRequest,
        timeout: Duration,
    ) -> Result<McpEventPoll, McpEventsCallError> {
        decode(
            server,
            wire::POLL_METHOD,
            call(
                &admit(&self.pool, server)?.peer,
                server,
                wire::POLL_METHOD,
                input.params(),
                timeout,
            )
            .await?,
        )
    }
    /// Open one push request. Read its window and explicitly close it before
    /// committing progress. Dropping it also attempts remote cancellation.
    pub async fn open(
        &self,
        server: &str,
        input: &McpEventRequest,
    ) -> Result<McpEventStream, McpEventsCallError> {
        let EventsConnection {
            policy,
            peer,
            handler,
            ..
        } = admit(&self.pool, server)?;
        let handle = peer
            .send_request_with_option(
                request(wire::STREAM_METHOD, input.params()),
                PeerRequestOptions::no_options(),
            )
            .await
            .map_err(|error| service_error(server, wire::STREAM_METHOD, Duration::ZERO, error))?;
        let streams = handler.event_streams();
        let subscription = handle.id.clone().into_json_value();
        let notifications = match streams.open(&subscription) {
            Ok(receiver) => receiver,
            Err(loss) => {
                let _ = peer.send_notification(cancel_notification(handle.id)).await;
                return Err(McpEventsCallError::ContinuityLost {
                    server: server.to_owned(),
                    loss,
                });
            }
        };
        Ok(McpEventStream {
            server: server.to_owned(),
            peer,
            streams,
            handle: Some(handle),
            notifications,
            settle: Duration::from_millis(policy.reorder_settle_ms),
            answered: false,
        })
    }
}

/// One open request; its routing and cancellation stay in the transport.
/// Read one window, then close. A partial window with a transport/decode fault
/// is an error and must never advance a host cursor.
pub struct McpEventStream {
    server: String,
    peer: Peer<RoleClient>,
    streams: Arc<EventStreams>,
    handle: Option<RequestHandle<RoleClient>>,
    notifications: mpsc::Receiver<(String, Value)>,
    settle: Duration,
    answered: bool,
}
impl McpEventStream {
    pub async fn read_window(
        &mut self,
        window: Duration,
        max_events: usize,
    ) -> Result<McpEventWindow, McpEventsCallError> {
        let deadline = tokio::time::Instant::now() + window;
        let mut closing = None;
        let mut out = McpEventWindow::default();
        let handle = self
            .handle
            .as_mut()
            .ok_or_else(|| McpEventsCallError::Disconnected {
                server: self.server.clone(),
            })?;
        loop {
            if let Some(loss) = self.streams.loss() {
                return Err(McpEventsCallError::ContinuityLost {
                    server: self.server.clone(),
                    loss,
                });
            }
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(closing.unwrap_or(deadline)) => {
                    if closing.is_some() { break; }
                    closing = Some(tokio::time::Instant::now() + self.settle);
                }
                item = self.notifications.recv() => {
                    let Some((method, params)) = item else {
                        if let Some(loss) = self.streams.loss() { return Err(McpEventsCallError::ContinuityLost { server: self.server.clone(), loss }); }
                        break;
                    };
                    let close = apply(&mut out, &method, params).map_err(|message| McpEventsCallError::Decode { server: self.server.clone(), method, message })?;
                    if close || out.events.len() >= max_events {
                        closing.get_or_insert_with(|| tokio::time::Instant::now() + self.settle);
                    }
                }
                result = &mut handle.rx, if !self.answered => {
                    self.answered = true;
                    match result {
                        Ok(Ok(_)) => { closing.get_or_insert_with(|| tokio::time::Instant::now() + self.settle); }
                        Ok(Err(error)) => return Err(service_error(&self.server, wire::STREAM_METHOD, window, error)),
                        Err(_) => return Err(McpEventsCallError::Transport { server: self.server.clone(), method: wire::STREAM_METHOD.to_owned(), message: "the stream closed".to_owned() }),
                    }
                }
            }
        }
        // An overflow racing the closing timer cannot publish a complete window.
        if let Some(loss) = self.streams.loss() {
            return Err(McpEventsCallError::ContinuityLost {
                server: self.server.clone(),
                loss,
            });
        }
        Ok(out)
    }
    /// Remove the route and cancel the remote request. Cancellation failures
    /// retain their typed protocol/transport cause.
    pub async fn close(mut self) -> Result<(), McpEventsCallError> {
        if let Some(handle) = self.handle.take() {
            self.streams.close(&handle.id.clone().into_json_value());
            self.peer
                .send_notification(cancel_notification(handle.id))
                .await
                .map_err(|error| {
                    service_error(&self.server, wire::STREAM_METHOD, Duration::ZERO, error)
                })?;
        }
        Ok(())
    }
}
impl Drop for McpEventStream {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.streams.close(&handle.id.clone().into_json_value());
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let peer = self.peer.clone();
                runtime.spawn(async move {
                    let _ = peer.send_notification(cancel_notification(handle.id)).await;
                });
            }
        }
    }
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
    peer: &Peer<RoleClient>,
    server: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, McpEventsCallError> {
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

fn cancel_notification(id: rmcp::model::RequestId) -> rmcp::model::ClientNotification {
    rmcp::model::CancelledNotification::new(rmcp::model::CancelledNotificationParam::new(
        Some(id),
        Some("the subscription's window closed".to_owned()),
    ))
    .into()
}

/// Fold one stream notification into `out`; whether it closes the window.
fn apply(out: &mut McpEventWindow, method: &str, params: Value) -> Result<bool, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{McpEventTransportPolicy, testing::McpEventsFixture};

    /// H6: a finite listing inside the host's declared page budget is accepted.
    #[tokio::test]
    async fn a_host_selected_65_page_budget_accepts_65_pages() {
        let world = tempfile::tempdir().unwrap();
        let fixture = McpEventsFixture::new(world.path());
        fixture.control(serde_json::json!({"list_pages": 65}));
        let mut config = fixture.server(McpEventTransportPolicy::default());
        config.data_limits.catalog_pages = 65;
        let pool = McpConnectionPool::connect(BTreeMap::from([("tickets".to_owned(), config)]))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match McpEventClient::new(Arc::clone(&pool))
                    .list("tickets", Duration::from_secs(5))
                    .await
                {
                    Err(McpEventsCallError::Disconnected { .. }) => tokio::task::yield_now().await,
                    result => break result.unwrap(),
                }
            }
        })
        .await
        .unwrap();
        pool.shutdown_all().await;
    }

    /// H6: routing must never silently present an overflowed window as complete.
    #[tokio::test]
    async fn orphan_overflow_is_reported_before_a_window_is_complete() {
        let world = tempfile::tempdir().unwrap();
        let fixture = McpEventsFixture::new(world.path());
        fixture.control(serde_json::json!({"orphan_notifications": 3}));
        let pool = McpConnectionPool::connect(BTreeMap::from([(
            "tickets".to_owned(),
            fixture.server(McpEventTransportPolicy {
                orphan_notifications: 2,
                reorder_settle_ms: 0,
            }),
        )]))
        .await
        .unwrap();
        let client = McpEventClient::new(Arc::clone(&pool));
        client
            .list("tickets", Duration::from_secs(5))
            .await
            .unwrap();
        let (_, _, service) = pool.events_connection("tickets").unwrap();
        let streams = service.unwrap().1.event_streams();
        tokio::time::timeout(Duration::from_secs(5), async {
            while streams.loss().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let result = client
            .open(
                "tickets",
                &McpEventRequest {
                    name: "ticket.created".to_owned(),
                    arguments: serde_json::Map::new(),
                    cursor: None,
                    max_events: 5,
                    max_age_ms: None,
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(McpEventsCallError::ContinuityLost {
                loss: McpEventContinuityLoss::NotificationOverflow { capacity: 2 },
                ..
            })
        ));
        pool.shutdown_all().await;
    }

    /// H6: a malformed push never publishes an apparently complete partial window.
    #[tokio::test]
    async fn a_malformed_notification_refuses_the_whole_window() {
        let world = tempfile::tempdir().unwrap();
        let fixture = McpEventsFixture::new(world.path());
        fixture.control(serde_json::json!({"pause_stream": true}));
        let pool = McpConnectionPool::connect(BTreeMap::from([(
            "tickets".to_owned(),
            fixture.server(McpEventTransportPolicy {
                orphan_notifications: 8,
                reorder_settle_ms: 10,
            }),
        )]))
        .await
        .unwrap();
        let mut stream = McpEventClient::new(Arc::clone(&pool))
            .open("tickets", &window_request())
            .await
            .unwrap();
        let id = stream.handle.as_ref().unwrap().id.clone().into_json_value();
        release_notifications(
            &pool,
            vec![
                notification(
                    &id,
                    wire::EVENT_NOTIFICATION,
                    serde_json::json!({
                        "eventId":"valid", "name":"ticket.created", "timestamp":"now",
                        "data":{}, "cursor":"paid-position",
                    }),
                ),
                notification(
                    &id,
                    wire::EVENT_NOTIFICATION,
                    serde_json::json!({"cursor":"bad"}),
                ),
                notification(
                    &id,
                    wire::TERMINATED_NOTIFICATION,
                    serde_json::json!({
                        "error":{"code":-32000, "message":"closed"},
                    }),
                ),
            ],
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while stream.notifications.len() != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            stream.notifications.len(),
            3,
            "the route received the entire batch"
        );
        let mut host_cursor = None;
        let result = stream.read_window(Duration::from_secs(1), 10).await;
        if let Ok(window) = &result {
            host_cursor.clone_from(&window.cursor);
        }
        assert!(
            matches!(result, Err(McpEventsCallError::Decode { server, method, .. })
            if server == "tickets" && method == wire::EVENT_NOTIFICATION)
        );
        assert_eq!(host_cursor, None);
        stream.close().await.unwrap();
        pool.shutdown_all().await;
    }

    /// H6: overflow after route installation poisons every reader, including
    /// a reader whose closing timer is already armed.
    #[tokio::test]
    async fn installed_route_overflow_refuses_a_closing_window_and_its_neighbor() {
        let world = tempfile::tempdir().unwrap();
        let fixture = McpEventsFixture::new(world.path());
        fixture.control(serde_json::json!({"pause_stream": true}));
        let pool = McpConnectionPool::connect(BTreeMap::from([(
            "tickets".to_owned(),
            fixture.server(McpEventTransportPolicy {
                orphan_notifications: 2,
                reorder_settle_ms: 100,
            }),
        )]))
        .await
        .unwrap();
        let client = McpEventClient::new(Arc::clone(&pool));
        let mut closing = client.open("tickets", &window_request()).await.unwrap();
        let mut neighbor = client.open("tickets", &window_request()).await.unwrap();
        let closing_id = closing
            .handle
            .as_ref()
            .unwrap()
            .id
            .clone()
            .into_json_value();
        let neighbor_id = neighbor
            .handle
            .as_ref()
            .unwrap()
            .id
            .clone()
            .into_json_value();
        assert_eq!(closing.streams.inner.lock_recover().routes.len(), 2);
        release_notifications(
            &pool,
            vec![notification(
                &closing_id,
                wire::TERMINATED_NOTIFICATION,
                serde_json::json!({
                    "error":{"code":-32000,"message":"closed"},
                }),
            )],
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while closing.notifications.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::pause();
        let mut closing_read = Box::pin(closing.read_window(Duration::from_secs(10), 10));
        assert!(matches!(
            futures_util::poll!(&mut closing_read),
            std::task::Poll::Pending
        ));
        // The notification was consumed and the settle timer is now armed.
        for n in 0..3 {
            neighbor.streams.deliver(
                &neighbor_id,
                wire::HEARTBEAT_NOTIFICATION.to_owned(),
                serde_json::json!({"cursor":n.to_string()}),
            );
        }
        let mut host_cursor = None;
        for result in [
            closing_read.await,
            neighbor.read_window(Duration::from_secs(10), 10).await,
        ] {
            if let Ok(window) = &result {
                host_cursor.clone_from(&window.cursor);
            }
            assert!(matches!(
                result,
                Err(McpEventsCallError::ContinuityLost {
                    loss: McpEventContinuityLoss::NotificationOverflow { capacity: 2 },
                    ..
                })
            ));
        }
        assert_eq!(host_cursor, None);
        tokio::time::resume();
        closing.close().await.unwrap();
        neighbor.close().await.unwrap();
        pool.shutdown_all().await;
    }

    #[derive(Default)]
    pub(super) struct PageHook {
        pub(super) reached: tokio::sync::Notify,
        pub(super) release: tokio::sync::Notify,
    }

    /// EVENT-OPERATION-PEER: one listing runs on the connection it admitted.
    /// A replacement installed between pages never receives the old peer's
    /// cursor, and the listing fails instead of joining two catalogs.
    #[tokio::test]
    async fn a_replacement_between_list_pages_never_receives_the_old_cursor() {
        let (old_world, new_world) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (old, new) = (
            McpEventsFixture::new(old_world.path()),
            McpEventsFixture::new(new_world.path()),
        );
        old.control(serde_json::json!({"list_pages": 2}));
        new.control(serde_json::json!({"list_pages": 2}));
        let pool = McpConnectionPool::connect(BTreeMap::from([(
            "tickets".to_owned(),
            old.server(McpEventTransportPolicy::default()),
        )]))
        .await
        .unwrap();
        let hook = Arc::new(PageHook::default());
        let mut client = McpEventClient::new(Arc::clone(&pool));
        client.between_pages = Some(Arc::clone(&hook));
        let listing =
            tokio::spawn(async move { client.list("tickets", Duration::from_secs(5)).await });
        hook.reached.notified().await;
        pool.attach(
            "tickets".to_owned(),
            new.server(McpEventTransportPolicy::default()),
        )
        .await
        .unwrap();
        assert!(
            pool.events_connection("tickets")
                .is_some_and(|(_, _, service)| service.is_some()),
            "the replacement is connected before the listing resumes"
        );
        hook.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), listing)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            new.requests_of(wire::LIST_METHOD),
            Vec::<Value>::new(),
            "the replacement received no part of the old listing"
        );
        assert!(
            matches!(result, Err(McpEventsCallError::Transport { ref server, ref method, .. })
                if server == "tickets" && method == wire::LIST_METHOD),
            "the listing fails on its own closed connection: {result:?}"
        );
        pool.shutdown_all().await;
    }

    fn window_request() -> McpEventRequest {
        McpEventRequest {
            name: "ticket.created".to_owned(),
            arguments: Default::default(),
            cursor: None,
            max_events: 10,
            max_age_ms: None,
        }
    }

    fn notification(id: &Value, method: &str, mut params: Value) -> Value {
        params["_meta"] = serde_json::json!({"io.modelcontextprotocol/subscriptionId": id});
        serde_json::json!({"method":method,"params":params})
    }

    async fn release_notifications(pool: &McpConnectionPool, notifications: Vec<Value>) {
        call(
            &admit(pool, "tickets").unwrap().peer,
            "tickets",
            "fixture/notifications",
            serde_json::json!({"notifications":notifications}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    }
}
