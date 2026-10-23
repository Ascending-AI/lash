//! The Codex WebSocket session cache, its leases, and the per-scope fallback
//! state.
//!
//! One responsibility: own which socket a Codex request runs on. A continuation
//! scope keeps at most one reusable connection; a lease hands it to exactly one
//! in-flight request and takes it back on release; an idle prune, an entry cap,
//! and a token-epoch eviction bound the cache; and a per-scope
//! fallback marker remembers that the WebSocket path failed so `Auto` can skip
//! it. `CodexWebSocketAttemptError` lives here because connecting is the first
//! place an attempt can fail.

use lash_protocol_types::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::llm::transport::{ProviderFailureKind, TurnFailureCode};
use lash_core::llm::types::ResponseContext;
use lash_llm_transport::TokenLease;
use lash_transport_failure::{LlmTransportError, TransportRetryVerdict};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::CodexProvider;
use super::continuation::CodexContinuation;
use super::shared::ResponsesStreamState;

/// Operational policy for reusable WebSocket streams and SSE fallback memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebSocketCachePolicy {
    /// Idle sockets expire after this duration. Zero disables idle reuse.
    pub idle_ttl: Duration,
    /// How long Auto remembers a failed WebSocket before trying again. Zero disables memory.
    pub fallback_ttl: Duration,
    /// Cache entry capacity. Only idle sockets are evicted; in-flight reservations
    /// may temporarily exceed the bound.
    /// Zero disables idle reuse.
    pub max_entries: usize,
    /// Best-effort close handshake deadline per idle socket during drain.
    pub close_timeout: Duration,
}
impl WebSocketCachePolicy {
    /// Standard cache: 300 s idle TTL, 60 s fallback TTL, 32 cache entries,
    /// and 2 s per close handshake. These historical choices have no workload
    /// measurements; the deadline prevents a dead peer from stalling drain.
    pub fn standard() -> Self {
        Self {
            idle_ttl: Duration::from_secs(300),
            fallback_ttl: Duration::from_secs(60),
            max_entries: 32,
            close_timeout: Duration::from_secs(2),
        }
    }
}
impl Default for WebSocketCachePolicy {
    fn default() -> Self {
        Self::standard()
    }
}

type CodexWsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Default)]
pub(super) struct CodexWebsocketSessionCache {
    pub(super) inner: Arc<Mutex<CodexWebsocketSessions>>,
}

impl std::fmt::Debug for CodexWebsocketSessionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (sessions_len, fallback_sessions_len) = self
            .inner
            .lock()
            .map(|sessions| (sessions.by_scope.len(), sessions.fallback_by_scope.len()))
            .unwrap_or_default();
        f.debug_struct("CodexWebsocketSessionCache")
            .field("sessions", &sessions_len)
            .field("fallback_sessions", &fallback_sessions_len)
            .finish()
    }
}

#[derive(Default)]
pub(super) struct CodexWebsocketSessions {
    pub(super) by_scope: HashMap<String, CodexWebsocketSessionEntry>,
    pub(super) fallback_by_scope: HashMap<String, CodexWebsocketFallbackState>,
    next_reservation_owner: u64,
}

impl CodexWebsocketSessions {
    /// Reserve `scope_key` for a new owner under the cache lock, replacing any
    /// entry there, and return the owner that must release it.
    fn reserve(
        &mut self,
        cache: &CodexWebsocketSessionCache,
        scope_key: String,
        token_epoch: u64,
    ) -> CodexWebsocketReservation {
        self.next_reservation_owner += 1;
        let owner = self.next_reservation_owner;
        self.by_scope.insert(
            scope_key.clone(),
            CodexWebsocketSessionEntry::Reserved { owner, token_epoch },
        );
        CodexWebsocketReservation {
            cache: cache.clone(),
            scope_key,
            owner,
            token_epoch,
        }
    }

    fn is_reserved_by(&self, reservation: &CodexWebsocketReservation) -> bool {
        matches!(
            self.by_scope.get(&reservation.scope_key),
            Some(CodexWebsocketSessionEntry::Reserved { owner, .. }) if *owner == reservation.owner
        )
    }
}

/// The obligation behind one `Reserved` cache slot. It exists from the moment
/// the slot is reserved, before any connection await, and moves into the lease
/// on success; dropping it anywhere (a failed or cancelled connect, a released
/// lease that is not kept) removes exactly its own slot and never a reservation
/// that replaced it.
pub(super) struct CodexWebsocketReservation {
    cache: CodexWebsocketSessionCache,
    scope_key: String,
    owner: u64,
    token_epoch: u64,
}

impl Drop for CodexWebsocketReservation {
    fn drop(&mut self) {
        let mut sessions = self.cache.inner.lock_recover();
        if sessions.is_reserved_by(self) {
            sessions.by_scope.remove(&self.scope_key);
        }
    }
}

pub(super) struct CodexWebsocketFallbackState {
    until: Option<Instant>,
    reason: String,
}

pub(super) enum CodexWebsocketSessionEntry {
    /// Leased to the live [`CodexWebsocketReservation`] whose `owner` this is.
    Reserved { owner: u64, token_epoch: u64 },
    Idle {
        connection: Box<CodexWsStream>,
        continuation: Option<CodexContinuation>,
        last_used: Instant,
        token_epoch: u64,
    },
}

#[derive(Clone, Copy)]
struct PrunableWebsocketSession {
    last_used: Instant,
    token_epoch: u64,
}

impl CodexWebsocketSessionEntry {
    fn prunable(&self) -> Option<PrunableWebsocketSession> {
        match self {
            Self::Reserved { .. } => None,
            Self::Idle {
                last_used,
                token_epoch,
                ..
            } => Some(PrunableWebsocketSession {
                last_used: *last_used,
                token_epoch: *token_epoch,
            }),
        }
    }
}

pub(super) struct CodexWebsocketLease {
    pub(super) websocket: CodexWsStream,
    /// The cache slot this lease owns; `None` for an ephemeral connection.
    pub(super) reservation: Option<CodexWebsocketReservation>,
    pub(super) reused: bool,
    pub(super) continuation: Option<CodexContinuation>,
}

impl CodexWebsocketLease {
    pub(super) fn reusable(&self) -> bool {
        self.reservation.is_some()
    }
}

/// Monotone attempt progress, ordered from no provider evidence to committed output.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum CodexAttemptProgress {
    BeforeSend,
    EventsSeen,
    OutputStarted,
}

pub(super) struct CodexWebSocketAttemptError {
    pub(super) error: LlmTransportError,
    progress: CodexAttemptProgress,
}

impl CodexWebSocketAttemptError {
    pub(super) fn before_send(error: LlmTransportError) -> Self {
        Self {
            error,
            progress: CodexAttemptProgress::BeforeSend,
        }
    }

    pub(super) fn during_stream(
        error: LlmTransportError,
        events_seen: bool,
        state: &ResponsesStreamState,
    ) -> Self {
        let progress = if state.output_started() {
            CodexAttemptProgress::OutputStarted
        } else if events_seen {
            CodexAttemptProgress::EventsSeen
        } else {
            CodexAttemptProgress::BeforeSend
        };
        Self { error, progress }
    }

    pub(super) fn progress(&self) -> CodexAttemptProgress {
        self.progress
    }

    pub(super) fn is_connection_limit_rejection(&self) -> bool {
        self.has_provider_code("websocket_connection_limit_reached")
    }

    fn has_provider_code(&self, spelling: &str) -> bool {
        self.error.code.as_ref().is_some_and(|code| {
            code.namespace().as_str() == "provider" && code.spelling() == spelling
        })
    }

    pub(super) fn is_stale_previous_response(&self) -> bool {
        self.has_provider_code("previous_response_not_found")
    }
}

impl CodexProvider {
    pub(super) fn prune_idle_websocket_sessions(
        sessions: &mut CodexWebsocketSessions,
        policy: &WebSocketCachePolicy,
    ) {
        let now = Instant::now();
        Self::prune_expired_websocket_fallbacks(sessions, now);
        // Dropping a cached WebSocketStream closes the socket. This prune path is
        // deliberately synchronous because the cache lock is provider-local.
        sessions.by_scope.retain(|_, entry| {
            entry.prunable().is_none_or(|idle| {
                !policy.idle_ttl.is_zero() && now.duration_since(idle.last_used) <= policy.idle_ttl
            })
        });
    }

    fn prune_expired_websocket_fallbacks(sessions: &mut CodexWebsocketSessions, now: Instant) {
        sessions
            .fallback_by_scope
            .retain(|_, fallback| fallback.until.is_none_or(|until| until > now));
    }

    pub(super) fn enforce_websocket_session_cache_cap(
        sessions: &mut CodexWebsocketSessions,
        policy: &WebSocketCachePolicy,
    ) {
        let excess = sessions.by_scope.len().saturating_sub(policy.max_entries);
        if excess == 0 {
            return;
        }

        let mut removable = sessions
            .by_scope
            .iter()
            .filter_map(|(scope_key, entry)| {
                entry
                    .prunable()
                    .map(|idle| (scope_key.clone(), idle.last_used))
            })
            .collect::<Vec<_>>();
        removable.sort_by_key(|(_, last_used)| *last_used);
        for (scope_key, _) in removable.into_iter().take(excess) {
            sessions.by_scope.remove(&scope_key);
        }
    }

    pub(super) fn evict_websocket_sessions_for_epoch(
        sessions: &mut CodexWebsocketSessions,
        token_epoch: u64,
    ) {
        sessions.by_scope.retain(|_, entry| {
            entry
                .prunable()
                .is_none_or(|idle| idle.token_epoch == token_epoch)
        });
    }

    /// Drain the WebSocket session cache, sending a proper Close frame on every
    /// idle cached connection before dropping it.
    ///
    /// This is the shutdown counterpart to the synchronous idle prune: the prune
    /// path drops streams (a TCP-level close), whereas a host-driven shutdown
    /// wants the WebSocket closing handshake. Busy entries are leased out to an
    /// in-flight `complete` call — their stream is not held in the cache — so
    /// this closes only idle, reusable sessions; the lease closes or re-caches
    /// its own connection on release. The cache lock is provider-local and
    /// non-async, so connections are taken out under the lock and closed after
    /// it is released.
    pub(super) async fn close_websocket_sessions(&self) {
        let connections: Vec<CodexWsStream> = {
            let mut sessions = self.websocket_sessions.inner.lock_recover();
            let drained = sessions
                .by_scope
                .drain()
                .filter_map(|(_, entry)| match entry {
                    CodexWebsocketSessionEntry::Reserved { .. } => None,
                    CodexWebsocketSessionEntry::Idle { connection, .. } => Some(*connection),
                })
                .collect();
            sessions.fallback_by_scope.clear();
            drained
        };
        for mut websocket in connections {
            // Best-effort: a peer that already vanished cannot receive the frame,
            // and shutdown must not fail because one socket is already gone. Bound
            // each close so a half-dead peer that never returns its Close frame
            // cannot stall the drain of the sockets still queued behind it.
            let _ = tokio::time::timeout(
                self.websocket_cache_policy.close_timeout,
                websocket.close(None),
            )
            .await;
        }
    }

    pub(super) fn clear_continuation(&self, call: &ResponseContext) {
        let scope_key = call.continuation_key();
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        if let Some(CodexWebsocketSessionEntry::Idle { continuation, .. }) =
            sessions.by_scope.get_mut(&scope_key)
        {
            *continuation = None;
        }
    }

    pub(super) fn websocket_fallback_reason(&self, call: &ResponseContext) -> Option<String> {
        let scope_key = call.continuation_key();
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        Self::prune_expired_websocket_fallbacks(&mut sessions, Instant::now());
        sessions
            .fallback_by_scope
            .get(&scope_key)
            .map(|fallback| fallback.reason.clone())
    }

    pub(super) fn record_websocket_fallback(
        &self,
        call: &ResponseContext,
        error: &LlmTransportError,
    ) {
        let scope_key = call.continuation_key();
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        let now = Instant::now();
        Self::prune_expired_websocket_fallbacks(&mut sessions, now);
        let reason = error
            .code
            .as_ref()
            .map(|code| format!("{}: {}", code.namespaced(), error.message))
            .unwrap_or_else(|| error.message.clone());
        sessions.fallback_by_scope.insert(
            scope_key,
            CodexWebsocketFallbackState {
                until: now.checked_add(self.websocket_cache_policy.fallback_ttl),
                reason,
            },
        );
    }

    pub(super) fn clear_websocket_fallback(&self, call: &ResponseContext) {
        let scope_key = call.continuation_key();
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        sessions.fallback_by_scope.remove(&scope_key);
    }

    #[allow(
        clippy::result_large_err,
        reason = "the attempt error carries the transport error plus lease evidence; boxing it would push the cost onto every caller"
    )]
    async fn connect_websocket(
        &self,
        call: &ResponseContext,
        connect_timeout: Option<Duration>,
        lease: &TokenLease,
    ) -> Result<CodexWsStream, CodexWebSocketAttemptError> {
        let mut ws_request =
            self.websocket_url
                .as_str()
                .into_client_request()
                .map_err(|error| {
                    CodexWebSocketAttemptError::before_send(LlmTransportError::new(format!(
                        "Failed to build Codex WebSocket request: {error}"
                    )))
                })?;
        let headers = ws_request.headers_mut();
        headers.insert(
            "Authorization",
            sensitive_websocket_header(&format!("Bearer {}", lease.token.secret().expose_secret()))
                .map_err(|error| {
                    CodexWebSocketAttemptError::before_send(LlmTransportError::new(format!(
                        "Invalid Codex WebSocket authorization header: {error}"
                    )))
                })?,
        );
        headers.insert(
            "OpenAI-Beta",
            HeaderValue::from_static(Self::CODEX_RESPONSES_WS_BETA),
        );
        headers.insert(
            "originator",
            HeaderValue::from_static(Self::CODEX_ORIGINATOR),
        );
        headers.insert(
            "User-Agent",
            HeaderValue::from_str(&Self::codex_user_agent()).map_err(|error| {
                CodexWebSocketAttemptError::before_send(LlmTransportError::new(format!(
                    "Invalid Codex WebSocket user-agent header: {error}"
                )))
            })?,
        );
        let session_value = HeaderValue::from_str(&call.scope.provider_session_affinity_key())
            .map_err(|error| {
                CodexWebSocketAttemptError::before_send(LlmTransportError::new(format!(
                    "Invalid Codex WebSocket session header: {error}"
                )))
            })?;
        let request_value = HeaderValue::from_str(&call.scope.request_id).map_err(|error| {
            CodexWebSocketAttemptError::before_send(LlmTransportError::new(format!(
                "Invalid Codex WebSocket request header: {error}"
            )))
        })?;
        headers.insert("session-id", session_value);
        headers.insert("x-client-request-id", request_value);
        if let Some(account_id) = lease.token.account() {
            headers.insert(
                "ChatGPT-Account-ID",
                sensitive_websocket_header(account_id.expose_secret()).map_err(|error| {
                    CodexWebSocketAttemptError::before_send(LlmTransportError::new(format!(
                        "Invalid Codex WebSocket account header: {error}"
                    )))
                })?,
            );
        }
        for (name, value) in self.extra_headers.iter() {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                CodexWebSocketAttemptError::before_send(LlmTransportError::new(format!(
                    "Invalid extra WebSocket header name: {error}"
                )))
            })?;
            let value = sensitive_websocket_header(value).map_err(|error| {
                CodexWebSocketAttemptError::before_send(LlmTransportError::new(format!(
                    "Invalid extra WebSocket header value: {error}"
                )))
            })?;
            headers.insert(name, value);
        }

        let connecting = connect_async(ws_request);
        let connect = match connect_timeout {
            Some(timeout) => tokio::time::timeout(timeout, connecting).await,
            None => Ok(connecting.await),
        }
        .map_err(|_| {
            CodexWebSocketAttemptError::before_send(
                LlmTransportError::new("Codex WebSocket connect timed out")
                    .with_kind(ProviderFailureKind::Timeout)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                    .with_lash_code(TurnFailureCode::WebsocketConnectTimeout),
            )
        })?;
        connect.map(|(websocket, _)| websocket).map_err(|error| {
            let status = match &error {
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    Some(response.status().as_u16())
                }
                _ => None,
            };
            let mut transport_error =
                LlmTransportError::new(format!("Codex WebSocket connect failed: {error}"))
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                    .with_lash_code(TurnFailureCode::WebsocketConnect);
            if let Some(status) = status {
                transport_error = transport_error.with_http_status(status);
            }
            CodexWebSocketAttemptError::before_send(transport_error)
        })
    }

    #[allow(
        clippy::result_large_err,
        reason = "the attempt error carries the transport error plus lease evidence; boxing it would push the cost onto every caller"
    )]
    pub(super) async fn acquire_websocket(
        &self,
        call: &ResponseContext,
        connect_timeout: Option<Duration>,
        lease: &TokenLease,
    ) -> Result<CodexWebsocketLease, CodexWebSocketAttemptError> {
        let scope_key = call.continuation_key();
        let token_epoch = lease.epoch;

        enum AcquireDecision {
            Reuse(Box<CodexWebsocketLease>),
            ConnectReusable(CodexWebsocketReservation),
            ConnectEphemeral,
        }

        let decision = {
            let mut sessions = self.websocket_sessions.inner.lock_recover();
            Self::prune_idle_websocket_sessions(&mut sessions, &self.websocket_cache_policy);
            Self::enforce_websocket_session_cache_cap(&mut sessions, &self.websocket_cache_policy);
            Self::evict_websocket_sessions_for_epoch(&mut sessions, token_epoch);
            match sessions.by_scope.remove(&scope_key) {
                Some(
                    reserved @ CodexWebsocketSessionEntry::Reserved {
                        token_epoch: reserved_epoch,
                        ..
                    },
                ) if reserved_epoch == token_epoch => {
                    sessions.by_scope.insert(scope_key, reserved);
                    AcquireDecision::ConnectEphemeral
                }
                Some(CodexWebsocketSessionEntry::Idle {
                    connection,
                    continuation,
                    ..
                }) => AcquireDecision::Reuse(Box::new(CodexWebsocketLease {
                    websocket: *connection,
                    reservation: Some(sessions.reserve(
                        &self.websocket_sessions,
                        scope_key,
                        token_epoch,
                    )),
                    reused: true,
                    continuation,
                })),
                Some(CodexWebsocketSessionEntry::Reserved { .. }) | None => {
                    AcquireDecision::ConnectReusable(sessions.reserve(
                        &self.websocket_sessions,
                        scope_key,
                        token_epoch,
                    ))
                }
            }
        };

        match decision {
            AcquireDecision::Reuse(lease) => Ok(*lease),
            AcquireDecision::ConnectEphemeral => {
                let websocket = self.connect_websocket(call, connect_timeout, lease).await?;
                Ok(CodexWebsocketLease {
                    websocket,
                    reservation: None,
                    reused: false,
                    continuation: None,
                })
            }
            AcquireDecision::ConnectReusable(reservation) => {
                // A failed or cancelled connect drops `reservation`, freeing the slot.
                let websocket = self.connect_websocket(call, connect_timeout, lease).await?;
                Ok(CodexWebsocketLease {
                    websocket,
                    reservation: Some(reservation),
                    reused: false,
                    continuation: None,
                })
            }
        }
    }

    pub(super) fn release_websocket_lease(
        &self,
        lease: CodexWebsocketLease,
        keep_connection: bool,
        continuation: Option<CodexContinuation>,
    ) {
        let CodexWebsocketLease {
            websocket,
            reservation,
            ..
        } = lease;
        // An ephemeral connection, or one not kept, closes here; dropping its
        // reservation frees the slot it owned.
        let Some(reservation) = reservation.filter(|_| keep_connection) else {
            return;
        };
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        if !sessions.is_reserved_by(&reservation) {
            return;
        }
        sessions.by_scope.insert(
            reservation.scope_key.clone(),
            CodexWebsocketSessionEntry::Idle {
                connection: Box::new(websocket),
                continuation,
                last_used: Instant::now(),
                token_epoch: reservation.token_epoch,
            },
        );
        Self::prune_idle_websocket_sessions(&mut sessions, &self.websocket_cache_policy);
        Self::enforce_websocket_session_cache_cap(&mut sessions, &self.websocket_cache_policy);
        // The slot is now idle, so dropping `reservation` after the lock is
        // released finds nothing of its own to remove.
        drop(sessions);
    }
}

fn sensitive_websocket_header(
    value: &str,
) -> Result<HeaderValue, tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue> {
    let mut header = HeaderValue::from_str(value)?;
    header.set_sensitive(true);
    Ok(header)
}
