//! The Codex WebSocket session cache, its leases, and the per-scope fallback
//! state.
//!
//! One responsibility: own which socket a Codex request runs on. A continuation
//! scope keeps at most one reusable connection; a lease hands it to exactly one
//! in-flight request and takes it back on release; an idle prune, an entry cap,
//! and a credential-generation eviction bound the cache; and a per-scope
//! fallback marker remembers that the WebSocket path failed so `Auto` can skip
//! it. `CodexWebSocketAttemptError` lives here because connecting is the first
//! place an attempt can fail.

use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind};
use lash_core::llm::types::LlmRequest;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::CodexProvider;
use super::continuation::CodexContinuation;
use super::credential::CodexCredential;

pub(super) const SESSION_WEBSOCKET_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const SESSION_WEBSOCKET_FALLBACK_TTL: Duration = Duration::from_secs(60);
pub(super) const MAX_SESSION_WEBSOCKET_CACHE_ENTRIES: usize = 32;
/// Per-socket bound on the closing handshake during shutdown drain. A half-dead
/// peer that never returns its Close frame must not stall the remaining cached
/// sockets, so each close is best-effort and abandoned after this elapses.
const SESSION_WEBSOCKET_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

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
}

pub(super) struct CodexWebsocketFallbackState {
    until: Instant,
    reason: String,
}

pub(super) struct CodexWebsocketSessionEntry {
    pub(super) connection: Option<CodexWsStream>,
    pub(super) continuation: Option<CodexContinuation>,
    pub(super) busy: bool,
    pub(super) last_used: Instant,
    pub(super) credential_generation: u64,
}

impl CodexWebsocketSessionEntry {
    fn reserved(credential_generation: u64) -> Self {
        Self {
            connection: None,
            continuation: None,
            busy: true,
            last_used: Instant::now(),
            credential_generation,
        }
    }
}

pub(super) struct CodexWebsocketLease {
    pub(super) websocket: CodexWsStream,
    pub(super) scope_key: Option<String>,
    pub(super) reusable: bool,
    pub(super) reused: bool,
    pub(super) continuation: Option<CodexContinuation>,
    pub(super) credential_generation: u64,
}

pub(super) struct CodexWebSocketAttemptError {
    pub(super) error: LlmTransportError,
    pub(super) events_seen: bool,
    pub(super) output_started: bool,
    pub(super) stale_previous_response: bool,
}

impl CodexProvider {
    fn remove_websocket_scope(&self, scope_key: &str) {
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        sessions.by_scope.remove(scope_key);
    }

    pub(super) fn prune_idle_websocket_sessions(sessions: &mut CodexWebsocketSessions) {
        let now = Instant::now();
        Self::prune_expired_websocket_fallbacks(sessions, now);
        // Dropping a cached WebSocketStream closes the socket. This prune path is
        // deliberately synchronous because the cache lock is provider-local.
        sessions.by_scope.retain(|_, entry| {
            entry.busy || now.duration_since(entry.last_used) <= SESSION_WEBSOCKET_CACHE_TTL
        });
    }

    fn prune_expired_websocket_fallbacks(sessions: &mut CodexWebsocketSessions, now: Instant) {
        sessions
            .fallback_by_scope
            .retain(|_, fallback| fallback.until > now);
    }

    pub(super) fn enforce_websocket_session_cache_cap(sessions: &mut CodexWebsocketSessions) {
        let excess = sessions
            .by_scope
            .len()
            .saturating_sub(MAX_SESSION_WEBSOCKET_CACHE_ENTRIES);
        if excess == 0 {
            return;
        }

        let mut removable = sessions
            .by_scope
            .iter()
            .filter(|(_, entry)| !entry.busy)
            .map(|(scope_key, entry)| (scope_key.clone(), entry.last_used))
            .collect::<Vec<_>>();
        removable.sort_by_key(|(_, last_used)| *last_used);
        for (scope_key, _) in removable.into_iter().take(excess) {
            sessions.by_scope.remove(&scope_key);
        }
    }

    pub(super) fn evict_websocket_sessions_for_generation(
        sessions: &mut CodexWebsocketSessions,
        credential_generation: u64,
    ) {
        sessions
            .by_scope
            .retain(|_, entry| entry.credential_generation == credential_generation);
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
                .filter_map(|(_, entry)| entry.connection)
                .collect();
            sessions.fallback_by_scope.clear();
            drained
        };
        for mut websocket in connections {
            // Best-effort: a peer that already vanished cannot receive the frame,
            // and shutdown must not fail because one socket is already gone. Bound
            // each close so a half-dead peer that never returns its Close frame
            // cannot stall the drain of the sockets still queued behind it.
            let _ =
                tokio::time::timeout(SESSION_WEBSOCKET_CLOSE_TIMEOUT, websocket.close(None)).await;
        }
    }

    pub(super) fn clear_continuation(&self, req: &LlmRequest) {
        let scope_key = req.continuation_key();
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        if let Some(entry) = sessions.by_scope.get_mut(&scope_key) {
            entry.continuation = None;
        }
    }

    pub(super) fn websocket_fallback_reason(&self, req: &LlmRequest) -> Option<String> {
        let scope_key = req.continuation_key();
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        Self::prune_expired_websocket_fallbacks(&mut sessions, Instant::now());
        sessions
            .fallback_by_scope
            .get(&scope_key)
            .map(|fallback| fallback.reason.clone())
    }

    pub(super) fn record_websocket_fallback(&self, req: &LlmRequest, error: &LlmTransportError) {
        let scope_key = req.continuation_key();
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        let now = Instant::now();
        Self::prune_expired_websocket_fallbacks(&mut sessions, now);
        let reason = error
            .code
            .as_deref()
            .map(|code| format!("{code}: {}", error.message))
            .unwrap_or_else(|| error.message.clone());
        sessions.fallback_by_scope.insert(
            scope_key,
            CodexWebsocketFallbackState {
                until: now + SESSION_WEBSOCKET_FALLBACK_TTL,
                reason,
            },
        );
    }

    pub(super) fn clear_websocket_fallback(&self, req: &LlmRequest) {
        let scope_key = req.continuation_key();
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        sessions.fallback_by_scope.remove(&scope_key);
    }

    async fn connect_websocket(
        &self,
        req: &LlmRequest,
        connect_timeout: Duration,
        credential: &CodexCredential,
    ) -> Result<CodexWsStream, CodexWebSocketAttemptError> {
        let mut ws_request =
            self.websocket_url
                .as_str()
                .into_client_request()
                .map_err(|error| CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Failed to build Codex WebSocket request: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                })?;
        let headers = ws_request.headers_mut();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", credential.access_token)).map_err(
                |error| CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Invalid Codex WebSocket authorization header: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                },
            )?,
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
                CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Invalid Codex WebSocket user-agent header: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                }
            })?,
        );
        let session_value = HeaderValue::from_str(&req.scope.session_id).map_err(|error| {
            CodexWebSocketAttemptError {
                error: LlmTransportError::new(format!(
                    "Invalid Codex WebSocket session header: {error}"
                )),
                events_seen: false,
                output_started: false,
                stale_previous_response: false,
            }
        })?;
        let request_value = HeaderValue::from_str(&req.scope.request_id).map_err(|error| {
            CodexWebSocketAttemptError {
                error: LlmTransportError::new(format!(
                    "Invalid Codex WebSocket request header: {error}"
                )),
                events_seen: false,
                output_started: false,
                stale_previous_response: false,
            }
        })?;
        headers.insert("session-id", session_value);
        headers.insert("x-client-request-id", request_value);
        if let Some(account_id) = credential.account_id.as_deref() {
            headers.insert(
                "ChatGPT-Account-ID",
                HeaderValue::from_str(account_id).map_err(|error| CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Invalid Codex WebSocket account header: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                })?,
            );
        }

        let connect = tokio::time::timeout(connect_timeout, connect_async(ws_request))
            .await
            .map_err(|_| CodexWebSocketAttemptError {
                error: LlmTransportError::new("Codex WebSocket connect timed out")
                    .with_kind(ProviderFailureKind::Timeout)
                    .retryable(true)
                    .with_code("websocket_connect_timeout"),
                events_seen: false,
                output_started: false,
                stale_previous_response: false,
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
                    .retryable(true)
                    .with_code("websocket_connect");
            if let Some(status) = status {
                transport_error = transport_error.with_status(status);
            }
            CodexWebSocketAttemptError {
                error: transport_error,
                events_seen: false,
                output_started: false,
                stale_previous_response: false,
            }
        })
    }

    pub(super) async fn acquire_websocket(
        &self,
        req: &LlmRequest,
        connect_timeout: Duration,
        credential: &CodexCredential,
        credential_generation: u64,
    ) -> Result<CodexWebsocketLease, CodexWebSocketAttemptError> {
        let scope_key = req.continuation_key();

        enum AcquireDecision {
            Reuse(Box<CodexWebsocketLease>),
            ConnectReusable(String),
            ConnectEphemeral,
        }

        let decision = {
            let mut sessions = self.websocket_sessions.inner.lock_recover();
            Self::prune_idle_websocket_sessions(&mut sessions);
            Self::enforce_websocket_session_cache_cap(&mut sessions);
            Self::evict_websocket_sessions_for_generation(&mut sessions, credential_generation);
            if let Some(entry) = sessions.by_scope.get_mut(&scope_key) {
                if entry.busy {
                    AcquireDecision::ConnectEphemeral
                } else if let Some(websocket) = entry.connection.take() {
                    entry.busy = true;
                    entry.last_used = Instant::now();
                    AcquireDecision::Reuse(Box::new(CodexWebsocketLease {
                        websocket,
                        scope_key: Some(scope_key),
                        reusable: true,
                        reused: true,
                        continuation: entry.continuation.clone(),
                        credential_generation,
                    }))
                } else {
                    *entry = CodexWebsocketSessionEntry::reserved(credential_generation);
                    AcquireDecision::ConnectReusable(scope_key.clone())
                }
            } else {
                sessions.by_scope.insert(
                    scope_key.clone(),
                    CodexWebsocketSessionEntry::reserved(credential_generation),
                );
                AcquireDecision::ConnectReusable(scope_key.clone())
            }
        };

        match decision {
            AcquireDecision::Reuse(lease) => Ok(*lease),
            AcquireDecision::ConnectEphemeral => {
                let websocket = self
                    .connect_websocket(req, connect_timeout, credential)
                    .await?;
                Ok(CodexWebsocketLease {
                    websocket,
                    scope_key: None,
                    reusable: false,
                    reused: false,
                    continuation: None,
                    credential_generation,
                })
            }
            AcquireDecision::ConnectReusable(scope_key) => {
                let websocket = match self
                    .connect_websocket(req, connect_timeout, credential)
                    .await
                {
                    Ok(websocket) => websocket,
                    Err(error) => {
                        self.remove_websocket_scope(&scope_key);
                        return Err(error);
                    }
                };
                Ok(CodexWebsocketLease {
                    websocket,
                    scope_key: Some(scope_key),
                    reusable: true,
                    reused: false,
                    continuation: None,
                    credential_generation,
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
        let Some(scope_key) = lease.scope_key else {
            return;
        };
        if !lease.reusable || !keep_connection {
            self.remove_websocket_scope(&scope_key);
            return;
        }
        let mut sessions = self.websocket_sessions.inner.lock_recover();
        sessions.by_scope.insert(
            scope_key,
            CodexWebsocketSessionEntry {
                connection: Some(lease.websocket),
                continuation,
                busy: false,
                last_used: Instant::now(),
                credential_generation: lease.credential_generation,
            },
        );
        Self::prune_idle_websocket_sessions(&mut sessions);
        Self::enforce_websocket_session_cache_cap(&mut sessions);
    }
}
