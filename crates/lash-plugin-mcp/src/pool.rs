//! Per-core MCP connection pool.
//!
//! [`McpConnectionPool`] holds one client per configured server and is shared
//! across every session built from the same [`lash_core::LashCore`]. The pool
//! attempts to connect each server eagerly when constructed, but a server
//! that is down never fails construction: the entry stays registered and a
//! background task retries with exponential backoff until it connects (or the
//! server is detached). A per-service lifecycle watcher detects a connection
//! that dies mid-life and re-establishes it the same way. Imported tool
//! definitions are kept across a disconnect so the tool catalog stays stable;
//! calls to a disconnected server fail loudly instead.
//!
//! The wire-level transport is provided by the official [`rmcp`] SDK.

use lash_sansio::sync::{LockResultExt, RwLockExt};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use futures_util::future::join_all;
#[cfg(test)]
use http::HeaderName;
use rmcp::ServiceError;
use rmcp::model::{
    CallToolRequestParams, ClientRequest, Content, PingRequest, ProtocolVersion, RawContent,
    Request, ResourceContents, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, QuitReason, RoleClient, RunningService};
use serde_json::{Value, json};
use tokio::time::timeout;

use lash_core::{
    AttachmentCreateMeta, MediaType, ToolCallOutput, ToolContext, ToolDefinition, ToolFailure,
    ToolFailureClass, ToolFailureSource, ToolResult, ToolRetryDisposition, ToolValue,
};
use lash_tool_support::ToolDefinitionLashlangExt;

#[cfg(test)]
use crate::config::McpCallPolicy;
use crate::config::{McpServerConfig, TimeoutDisconnectPolicy};
use crate::error::McpError;
use crate::host::{
    LashMcpClientHandler, McpHostRequestTasks, McpHostServices, McpToolListChangedHandler,
};
use crate::naming;
#[cfg(test)]
use crate::service_lifecycle::build_http_headers;
use crate::service_lifecycle::{
    ConnectedService, McpService, ServiceQuit, StdioChildGuard, cancel_running_service,
    connect_service, equal_jitter, is_connection_loss, stop_service,
};

/// Shared, per-core connection pool. Wrapped in `Arc` and cloned into each
/// session plugin instance.
pub struct McpConnectionPool {
    entries: RwLock<BTreeMap<String, Arc<McpEntry>>>,
    host_services: McpHostServices,
    shut_down: AtomicBool,
}

/// Connection status of one configured server, for host/UI observability.
#[derive(Clone, Debug)]
pub struct McpServerStatus {
    pub server_name: String,
    pub connected: bool,
    /// Most recent connection error; cleared when a connect succeeds.
    pub last_error: Option<String>,
    /// Number of tools imported from the server's last successful discovery.
    pub tool_count: usize,
    /// Whether the configured reconnect-attempt budget has been exhausted.
    pub reconnect_exhausted: bool,
}

struct McpEntry {
    server_name: String,
    config: McpServerConfig,
    host_services: McpHostServices,
    /// `None` while disconnected. Once connected this retains the peer and
    /// exact-service teardown controls while a watcher owns rmcp's join handle.
    service: tokio::sync::Mutex<Option<McpService>>,
    /// Cached, prefixed tool definitions for this server, refreshed on every
    /// successful (re)connect and kept across a disconnect so the tool
    /// surface stays stable. Keys are the prefixed names
    /// (`mcp__<server>__<tool>`).
    imported_tools: RwLock<BTreeMap<String, ImportedTool>>,
    connected: AtomicBool,
    last_error: RwLock<Option<String>>,
    /// Set on detach/shutdown; stops any background reconnect loop.
    cancelled: AtomicBool,
    /// Guards against spawning concurrent reconnect loops for one entry.
    connecting: AtomicBool,
    /// Applies randomized delay to the entry-owned reconnect ceiling. Kept as
    /// a seam so pacing tests can observe ceilings without wall-clock sleeps.
    reconnect_jitter: Arc<dyn Fn(Duration) -> Duration + Send + Sync>,
    /// Persists across reconnect tasks so short-lived successful handshakes do
    /// not restart a crash loop at the initial delay.
    reconnect_backoff_ms: AtomicU64,
    /// Allocates an identity for every connection attempt, including attempts
    /// that fail before publishing a service.
    generation_allocator: AtomicU64,
    /// Set when a bounded reconnect loop spends its final attempt.
    reconnect_exhausted: AtomicBool,
    /// Consecutive idle timeouts since the last successful tool call.
    consecutive_timeouts: AtomicU64,
    /// Monotonically identifies the service currently installed in `service`.
    /// Late work from an older service may observe this but cannot disconnect
    /// its replacement.
    service_generation: AtomicU64,
    /// Keeps the protocol-version degradation warning to once per server.
    ping_degrade_warned: AtomicBool,
    /// Guards against spawning more than one optional keepalive loop.
    keepalive_started: AtomicBool,
    /// Wakes a sleeping or establishing reconnect when teardown wins.
    cancelled_notify: Arc<tokio::sync::Notify>,
    /// Wakes teardown after a background loop has relinquished the entry.
    reconnect_idle_notify: tokio::sync::Notify,
    #[cfg(test)]
    reconnect_publish_hook: Option<Arc<policy_tests::ReconnectPublishHook>>,
}

struct McpToolListRefresh {
    entry: Weak<McpEntry>,
    service_generation: u64,
}

#[async_trait::async_trait]
impl McpToolListChangedHandler for McpToolListRefresh {
    async fn refresh_tools(&self, peer: Peer<RoleClient>) {
        if let Some(entry) = self.entry.upgrade() {
            entry.refresh_tools(peer, self.service_generation).await;
        }
    }
}

#[derive(Clone)]
struct ImportedTool {
    /// The native MCP tool name as advertised by the server (before
    /// prefixing/normalisation).
    original_name: String,
    definition: ToolDefinition,
}

impl McpConnectionPool {
    /// Construct an empty pool.
    pub fn empty() -> Self {
        Self::empty_with_host_services(McpHostServices::default())
    }

    pub(crate) fn empty_with_host_services(host_services: McpHostServices) -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
            host_services,
            shut_down: AtomicBool::new(false),
        }
    }

    /// Build a pool for the configured servers. Every server is tried eagerly
    /// in parallel so tools are available immediately when servers are up, but a
    /// connection failure never aborts construction: the entry stays
    /// registered and reconnects in the background. Only configuration errors
    /// (a misconfigured server, not an outage) fail the build.
    pub async fn connect(
        servers: BTreeMap<String, McpServerConfig>,
    ) -> Result<Arc<Self>, McpError> {
        Self::connect_with_host_services(servers, McpHostServices::default()).await
    }

    pub(crate) async fn connect_with_host_services(
        servers: BTreeMap<String, McpServerConfig>,
        host_services: McpHostServices,
    ) -> Result<Arc<Self>, McpError> {
        validate_unique_server_prefixes(servers.keys().map(String::as_str))?;
        let pool = Arc::new(Self::empty_with_host_services(host_services));
        let mut entries = Vec::with_capacity(servers.len());
        for (name, config) in servers {
            config.validate(&name)?;
            let entry = Arc::new(McpEntry::new(
                name.clone(),
                config,
                pool.host_services.clone(),
            ));
            if let Err((rejected, error)) = pool.install(name.clone(), Arc::clone(&entry)) {
                rejected.cancel();
                rejected.shutdown().await;
                return Err(error);
            }
            entries.push((name, entry));
        }
        join_all(entries.into_iter().map(|(name, entry)| async move {
            let connect_result = entry.establish().await;
            let _ = entry.spawn_keepalive_loop();
            if let Err(err) = connect_result {
                tracing::warn!(
                    server = %name,
                    error = %err,
                    "MCP server unavailable at startup; retrying in the background"
                );
                entry.spawn_reconnect_loop();
            }
        }))
        .await;
        Ok(pool)
    }

    /// Add (or replace) one server in the pool. Like initial pool construction,
    /// attach registers the entry before an eager connection attempt and keeps
    /// retrying startup outages in the background. Only configuration and pool
    /// lifecycle errors fail the attach.
    pub async fn attach(
        self: &Arc<Self>,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<(), McpError> {
        if self.shut_down.load(Ordering::SeqCst) {
            return Err(McpError::Protocol(
                "MCP connection pool has already shut down".to_string(),
            ));
        }
        config.validate(&server_name)?;
        self.validate_server_prefix_available(&server_name)?;
        let entry = Arc::new(McpEntry::new(
            server_name.clone(),
            config,
            self.host_services.clone(),
        ));
        let previous = match self.install(server_name.clone(), Arc::clone(&entry)) {
            Ok(previous) => previous,
            Err((rejected, error)) => {
                rejected.cancel();
                rejected.shutdown().await;
                return Err(error);
            }
        };
        if let Some(previous) = previous {
            previous.cancel();
            previous.shutdown().await;
        }
        let connect_result = entry.establish().await;
        let _ = entry.spawn_keepalive_loop();
        if let Err(err) = connect_result {
            if self.shut_down.load(Ordering::SeqCst) || entry.cancelled.load(Ordering::SeqCst) {
                return Err(McpError::Protocol(
                    "MCP connection pool shut down while attaching a server".to_string(),
                ));
            }
            tracing::warn!(
                server = %server_name,
                error = %err,
                "MCP server unavailable during attach; retrying in the background"
            );
            entry.spawn_reconnect_loop();
        }
        Ok(())
    }

    /// Remove and shut down one server.
    pub async fn detach(self: &Arc<Self>, server_name: &str) -> Result<(), McpError> {
        let removed = {
            let mut entries = self.entries.write_recover();
            entries.remove(server_name)
        };
        if let Some(entry) = removed {
            entry.cancel();
            entry.shutdown().await;
        }
        Ok(())
    }

    /// Register an entry and return any previous entry under the same name.
    fn install(
        &self,
        server_name: String,
        entry: Arc<McpEntry>,
    ) -> Result<Option<Arc<McpEntry>>, (Arc<McpEntry>, McpError)> {
        let previous = {
            let mut entries = self.entries.write_recover();
            if self.shut_down.load(Ordering::SeqCst) {
                return Err((
                    entry,
                    McpError::Protocol("MCP connection pool has already shut down".to_string()),
                ));
            }
            if let Some((existing_server, prefix)) =
                conflicting_server_prefix(entries.keys().map(String::as_str), &server_name)
            {
                return Err((
                    entry,
                    McpError::Config(prefix_collision_message(
                        existing_server,
                        &server_name,
                        &prefix,
                    )),
                ));
            }
            entries.insert(server_name, entry)
        };
        Ok(previous)
    }

    fn validate_server_prefix_available(&self, server_name: &str) -> Result<(), McpError> {
        let entries = self.entries.read_recover();
        if let Some((existing_server, prefix)) =
            conflicting_server_prefix(entries.keys().map(String::as_str), server_name)
        {
            return Err(McpError::Config(prefix_collision_message(
                existing_server,
                server_name,
                &prefix,
            )));
        }
        Ok(())
    }

    /// Connection status of every configured server.
    pub fn server_statuses(&self) -> Vec<McpServerStatus> {
        let guard = self.entries.read_recover();
        guard
            .values()
            .map(|entry| McpServerStatus {
                server_name: entry.server_name.clone(),
                connected: entry.connected.load(Ordering::SeqCst),
                last_error: entry.last_error.read_recover().clone(),
                tool_count: entry.imported_tools.read_recover().len(),
                reconnect_exhausted: entry.reconnect_exhausted.load(Ordering::SeqCst),
            })
            .collect()
    }

    /// Notify every connected server that the host's roots may have changed.
    ///
    /// Disconnected servers are skipped: they receive the current list from
    /// the same provider after reconnecting and issuing `roots/list`.
    #[allow(
        deprecated,
        reason = "MCP 2025-11-25 still defines roots notifications"
    )]
    pub async fn notify_roots_changed(&self) -> Result<(), McpError> {
        if !self.host_services.has_roots() {
            return Err(McpError::Config(
                "cannot notify MCP roots changes without a roots provider".to_string(),
            ));
        }
        if self.shut_down.load(Ordering::SeqCst) {
            return Err(McpError::Protocol(
                "MCP connection pool has already shut down".to_string(),
            ));
        }

        let entries: Vec<Arc<McpEntry>> = self
            .entries
            .read_recover()
            .values()
            .filter(|entry| entry.connected.load(Ordering::SeqCst))
            .cloned()
            .collect();
        let mut peers = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(service) = entry.service.lock().await.as_ref() {
                peers.push((entry.server_name.clone(), service.peer().clone()));
            }
        }
        let failures = collect_notification_failures(peers, |peer| async move {
            peer.notify_roots_list_changed().await
        })
        .await;
        if !failures.is_empty() {
            return Err(McpError::Protocol(format!(
                "failed to notify MCP servers that roots changed: {}",
                failures.join("; ")
            )));
        }
        Ok(())
    }

    /// All advertised tools across every server, with `mcp__<server>__<tool>`
    /// prefixed names. Cheap — these are precomputed `ToolDefinition` clones.
    /// Includes tools of currently disconnected servers (last successful
    /// discovery) so the tool catalog stays stable across an outage.
    pub fn advertised_tools(&self) -> Vec<ToolDefinition> {
        let guard = self.entries.read_recover();
        guard
            .values()
            .flat_map(|entry| {
                entry
                    .imported_tools
                    .read_recover()
                    .values()
                    .map(|tool| tool.definition.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Route a prefixed tool call (`mcp__<server>__<tool>`) to the appropriate
    /// server and translate its result back to `ToolResult`.
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        args: &Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        if self.shut_down.load(Ordering::SeqCst) {
            return pool_shut_down_failure();
        }
        let (entry, original_name) = match self.lookup(prefixed_name).await {
            Some(found) => found,
            None => {
                return ToolResult::err_fmt(format!("Unknown MCP tool: {prefixed_name}"));
            }
        };

        let call_timeout = entry.config.call_timeout();
        let server_name = entry.server_name.clone();
        let arguments = match args {
            Value::Object(map) => Some(map.clone()),
            Value::Null => None,
            other => {
                return ToolResult::err_fmt(format!(
                    "MCP tool `{prefixed_name}` expected an object argument, got {}",
                    other
                ));
            }
        };

        // Clone the peer handle while briefly holding the lock, then release it
        // before issuing the request. `rmcp::Peer` is a cheap, cloneable handle
        // (an mpsc sender plus an internal request-id provider) that supports
        // concurrent in-flight requests, so holding the mutex across the network
        // await would needlessly serialize tool calls to the same server and
        // risk a guard held across `.await`.
        let (peer, service_generation) = {
            let service_guard = entry.service.lock().await;
            match service_guard.as_ref() {
                Some(service) => (
                    service.peer().clone(),
                    entry.service_generation.load(Ordering::SeqCst),
                ),
                None => {
                    if entry.cancelled.load(Ordering::SeqCst) {
                        return ToolResult::failure(ToolFailure {
                            class: ToolFailureClass::Unavailable,
                            code: "mcp_server_unavailable".into(),
                            message: format!(
                                "MCP server `{server_name}` is unavailable because its pool entry is shutting down"
                            ),
                            source: ToolFailureSource::Plugin,
                            retry: ToolRetryDisposition::Never,
                            raw: None,
                        });
                    }
                    if entry.reconnect_exhausted.load(Ordering::SeqCst) {
                        let last_error = entry.last_error.read_recover().clone();
                        return ToolResult::failure(ToolFailure {
                            class: ToolFailureClass::Unavailable,
                            code: "mcp_reconnect_exhausted".into(),
                            message: last_error.unwrap_or_else(|| {
                                format!(
                                    "MCP server `{server_name}` reconnect attempts exhausted; no background recovery is active"
                                )
                            }),
                            source: ToolFailureSource::Plugin,
                            retry: ToolRetryDisposition::Never,
                            raw: None,
                        });
                    }
                    let previous_error = entry.last_error.read_recover().clone();
                    let dispatch_error = match previous_error {
                        Some(previous_error) if previous_error.contains("before tool dispatch") => {
                            previous_error
                        }
                        Some(previous_error) => format!(
                            "MCP server `{server_name}` was disconnected before tool dispatch \
                             (reconnecting in the background; last error: {previous_error})"
                        ),
                        None => format!(
                            "MCP server `{server_name}` was disconnected before tool dispatch"
                        ),
                    };
                    *entry.last_error.write_recover() = Some(dispatch_error.clone());
                    let message = McpError::Protocol(dispatch_error);
                    return ToolResult::retryable_failure(
                        ToolFailureClass::Unavailable,
                        "mcp_server_unavailable",
                        message.to_string(),
                        Some(entry.config.reconnect_initial_backoff().as_millis() as u64),
                    );
                }
            }
        };

        // `RunningService::is_closed()` only reflects explicit cancellation or
        // whether its join handle was taken; a dead child transport can still
        // report open. Health checks must use the peer transport sender.
        if peer.is_transport_closed() {
            let cause =
                format!("MCP server `{server_name}` transport was closed before tool dispatch");
            entry
                .mark_disconnected(cause.clone(), service_generation)
                .await;
            return ToolResult::retryable_failure(
                ToolFailureClass::Unavailable,
                "mcp_connection_lost",
                format!("{cause}; reconnecting in the background"),
                Some(entry.config.reconnect_initial_backoff().as_millis() as u64),
            );
        }

        let mut params = CallToolRequestParams::new(original_name);
        params.arguments = arguments;
        let mut options = PeerRequestOptions::with_timeout(call_timeout)
            .with_max_total_timeout(entry.config.call_max_total_timeout());
        if entry.config.reset_call_timeout_on_progress() {
            options = options.reset_timeout_on_progress();
        }
        let response = match peer
            .send_cancellable_request(
                ClientRequest::CallToolRequest(Request::new(params)),
                options,
            )
            .await
        {
            Ok(handle) => handle.await_response().await,
            Err(err) => Err(err),
        };

        match response {
            Ok(ServerResult::CallToolResult(result)) => {
                entry.record_call_success(service_generation).await;
                tool_result_from_rmcp(result, context, entry.config.binary_content_attachments())
                    .await
            }
            Ok(_) => ToolResult::err_fmt(McpError::Protocol(
                ServiceError::UnexpectedResponse.to_string(),
            )),
            Err(ServiceError::Timeout { timeout }) => {
                entry
                    .handle_call_timeout(&peer, service_generation, timeout)
                    .await
            }
            Err(ServiceError::Cancelled { reason }) => ToolResult::cancelled(format!(
                "MCP tool call on `{server_name}` was cancelled{}",
                reason
                    .as_deref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            )),
            Err(err) => {
                if is_connection_loss(&err) {
                    let cause = format!("MCP server `{server_name}` connection lost: {err}");
                    entry
                        .mark_disconnected(cause.clone(), service_generation)
                        .await;
                    if entry.cancelled.load(Ordering::SeqCst) {
                        return ToolResult::failure(ToolFailure {
                            class: ToolFailureClass::Unavailable,
                            code: "mcp_server_unavailable".into(),
                            message: McpError::Protocol(format!(
                                "MCP server `{server_name}` connection lost during pool shutdown: {err}"
                            ))
                            .to_string(),
                            source: ToolFailureSource::Plugin,
                            retry: ToolRetryDisposition::Never,
                            raw: None,
                        });
                    }
                    return ToolResult::retryable_failure(
                        ToolFailureClass::Unavailable,
                        "mcp_connection_lost",
                        McpError::Protocol(format!("{cause}; reconnecting in the background"))
                            .to_string(),
                        Some(entry.config.reconnect_initial_backoff().as_millis() as u64),
                    );
                }
                ToolResult::err_fmt(McpError::Protocol(err.to_string()))
            }
        }
    }

    async fn lookup(&self, prefixed_name: &str) -> Option<(Arc<McpEntry>, String)> {
        let guard = self.entries.read_recover();
        for entry in guard.values() {
            let original_name = entry
                .imported_tools
                .read_recover()
                .get(prefixed_name)
                .map(|tool| tool.original_name.clone());
            if let Some(original_name) = original_name {
                return Some((Arc::clone(entry), original_name));
            }
        }
        None
    }

    /// Tear down all connections in parallel. Call this before dropping the
    /// pool for a graceful shutdown; `Drop` itself cannot await. Each entry is
    /// cancellation-notified before any entry begins teardown, and each explicit
    /// rmcp service cancellation is bounded by rmcp's three-second grace plus
    /// transport-task drain.
    ///
    /// The first caller wins and completes teardown. A concurrent or later
    /// caller returns immediately.
    pub async fn shutdown_all(&self) {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return;
        }
        let entries: Vec<Arc<McpEntry>> = {
            let mut guard = self.entries.write_recover();
            std::mem::take(&mut *guard).into_values().collect()
        };
        for entry in &entries {
            entry.cancel();
        }
        join_all(entries.iter().map(|entry| entry.shutdown())).await;
    }
}

async fn collect_notification_failures<T, E, F, Fut>(
    targets: Vec<(String, T)>,
    notify: F,
) -> Vec<String>
where
    E: std::fmt::Display,
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    join_all(targets.into_iter().map(|(server_name, target)| {
        let notification = notify(target);
        async move {
            notification
                .await
                .err()
                .map(|error| format!("`{server_name}`: {error}"))
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

fn pool_shut_down_failure() -> ToolResult {
    ToolResult::failure(ToolFailure {
        class: ToolFailureClass::Unavailable,
        code: "mcp_pool_shut_down".into(),
        message: "MCP connection pool has shut down".to_string(),
        source: ToolFailureSource::Plugin,
        retry: ToolRetryDisposition::Never,
        raw: None,
    })
}

fn validate_unique_server_prefixes<'a>(
    server_names: impl IntoIterator<Item = &'a str>,
) -> Result<(), McpError> {
    let mut prefixes = BTreeMap::<String, &'a str>::new();
    for server_name in server_names {
        let prefix = naming::normalize_identifier(server_name);
        if let Some(existing_server) = prefixes.insert(prefix.clone(), server_name) {
            return Err(McpError::Config(prefix_collision_message(
                existing_server,
                server_name,
                &prefix,
            )));
        }
    }
    Ok(())
}

fn conflicting_server_prefix<'a>(
    existing_server_names: impl IntoIterator<Item = &'a str>,
    incoming_server: &str,
) -> Option<(&'a str, String)> {
    let incoming_prefix = naming::normalize_identifier(incoming_server);
    existing_server_names
        .into_iter()
        .find(|existing_server| {
            *existing_server != incoming_server
                && naming::normalize_identifier(existing_server) == incoming_prefix
        })
        .map(|existing_server| (existing_server, incoming_prefix))
}

fn prefix_collision_message(existing_server: &str, incoming_server: &str, prefix: &str) -> String {
    format!(
        "MCP servers `{existing_server}` and `{incoming_server}` normalize to the same prefix `{prefix}`"
    )
}

impl McpEntry {
    fn new(server_name: String, config: McpServerConfig, host_services: McpHostServices) -> Self {
        let reconnect_backoff_ms = config.reconnect_initial_backoff().as_millis() as u64;
        Self {
            server_name,
            config,
            host_services,
            service: tokio::sync::Mutex::new(None),
            imported_tools: RwLock::new(BTreeMap::new()),
            connected: AtomicBool::new(false),
            last_error: RwLock::new(None),
            cancelled: AtomicBool::new(false),
            connecting: AtomicBool::new(false),
            reconnect_jitter: Arc::new(equal_jitter),
            reconnect_backoff_ms: AtomicU64::new(reconnect_backoff_ms),
            generation_allocator: AtomicU64::new(0),
            reconnect_exhausted: AtomicBool::new(false),
            consecutive_timeouts: AtomicU64::new(0),
            service_generation: AtomicU64::new(0),
            ping_degrade_warned: AtomicBool::new(false),
            keepalive_started: AtomicBool::new(false),
            cancelled_notify: Arc::new(tokio::sync::Notify::new()),
            reconnect_idle_notify: tokio::sync::Notify::new(),
            #[cfg(test)]
            reconnect_publish_hook: None,
        }
    }

    #[cfg(test)]
    fn with_reconnect_jitter(
        mut self,
        reconnect_jitter: Arc<dyn Fn(Duration) -> Duration + Send + Sync>,
    ) -> Self {
        self.reconnect_jitter = reconnect_jitter;
        self
    }

    #[cfg(test)]
    fn with_reconnect_publish_hook(
        mut self,
        hook: Arc<policy_tests::ReconnectPublishHook>,
    ) -> Self {
        self.reconnect_publish_hook = Some(hook);
        self
    }

    #[cfg(test)]
    async fn pause_after_reconnect_publish(&self) {
        if let Some(hook) = &self.reconnect_publish_hook
            && hook.armed.swap(false, Ordering::SeqCst)
        {
            hook.published.notify_one();
            hook.release.notified().await;
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancelled_notify.notify_waiters();
    }

    fn reconnect_backoff(&self) -> Duration {
        Duration::from_millis(self.reconnect_backoff_ms.load(Ordering::SeqCst))
    }

    fn advance_reconnect_backoff(&self) {
        let max_ms = self.config.reconnect_max_backoff().as_millis() as u64;
        let _ =
            self.reconnect_backoff_ms
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    Some(current.saturating_mul(2).min(max_ms))
                });
    }

    async fn reset_reconnect_backoff_if_current(&self, observed_generation: u64) {
        let guard = self.service.lock().await;
        if self.service_generation.load(Ordering::SeqCst) == observed_generation && guard.is_some()
        {
            self.reconnect_backoff_ms.store(
                self.config.reconnect_initial_backoff().as_millis() as u64,
                Ordering::SeqCst,
            );
        }
    }

    /// A connection earns a backoff reset only after remaining live for the
    /// host-configured maximum reconnect window. This adds no separate timing
    /// policy and distinguishes a healthy service from a successful handshake.
    fn reconnect_healthy_dwell(&self) -> Duration {
        self.config.reconnect_max_backoff()
    }

    /// One connection attempt: handshake, tool discovery, then swap in the
    /// fresh service and definitions. Records the error on failure so status
    /// and call-time messages can report it.
    async fn establish(self: &Arc<Self>) -> Result<(), McpError> {
        match self.try_connect().await {
            Ok(()) => Ok(()),
            Err(err) => {
                *self.last_error.write_recover() = Some(err.to_string());
                Err(err)
            }
        }
    }

    async fn try_connect(self: &Arc<Self>) -> Result<(), McpError> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(McpError::Protocol(format!(
                "MCP connection for `{}` was cancelled",
                self.server_name
            )));
        }
        let previous_generation = self
            .generation_allocator
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |generation| {
                generation.checked_add(1)
            })
            .expect("MCP service generation exhausted");
        let next_generation = previous_generation + 1;
        let tool_list_refresh = Arc::new(McpToolListRefresh {
            entry: Arc::downgrade(self),
            service_generation: next_generation,
        });
        let connected_service = timeout(
            self.config.startup_timeout(),
            connect_service(
                &self.server_name,
                &self.config,
                self.host_services.clone(),
                tool_list_refresh,
            ),
        )
        .await
        .map_err(|_| McpError::StartupTimeout {
            server: self.server_name.clone(),
            timeout_ms: self.config.startup_timeout().as_millis() as u64,
        })??;
        let ConnectedService {
            running: service,
            stdio_child,
        } = connected_service;

        if self.cancelled.load(Ordering::SeqCst) {
            cancel_running_service(service).await;
            if let Some(child) = &stdio_child {
                child.reap_after_ungraceful_drop();
            }
            return Err(McpError::Protocol(format!(
                "MCP connection for `{}` was cancelled during startup",
                self.server_name
            )));
        }

        // Bound the discovery call so a server that completes the handshake but
        // then hangs on `tools/list` surfaces a timeout instead of blocking the
        // connect attempt indefinitely. Discovery happens during startup, so
        // the startup budget is the natural bound.
        let discovery_timeout = self.config.startup_timeout();
        let peer = service.peer().clone();
        let discovery = timeout(discovery_timeout, peer.list_all_tools());
        tokio::pin!(discovery);
        let tools_result = tokio::select! {
            result = &mut discovery => match result {
                Err(_) => Err(McpError::StartupTimeout {
                    server: self.server_name.clone(),
                    timeout_ms: discovery_timeout.as_millis() as u64,
                }),
                Ok(Err(error)) => Err(McpError::Protocol(format!(
                    "list_tools failed: {error}"
                ))),
                Ok(Ok(tools)) => Ok(tools),
            },
            () = self.cancelled_notify.notified() => {
                if self.cancelled.load(Ordering::SeqCst) {
                    cancel_running_service(service).await;
                    if let Some(child) = &stdio_child {
                        child.reap_after_ungraceful_drop();
                    }
                    return Err(McpError::Protocol(format!(
                        "MCP connection for `{}` was cancelled during discovery",
                        self.server_name
                    )));
                }
                unreachable!("the discovery notification is emitted only for cancellation");
            }
        };
        let tools = match tools_result {
            Ok(tools) => tools,
            Err(error) => {
                cancel_running_service(service).await;
                if let Some(child) = &stdio_child {
                    child.reap_after_ungraceful_drop();
                }
                return Err(error);
            }
        };

        if self.cancelled.load(Ordering::SeqCst) {
            cancel_running_service(service).await;
            if let Some(child) = &stdio_child {
                child.reap_after_ungraceful_drop();
            }
            return Err(McpError::Protocol(format!(
                "MCP connection for `{}` was cancelled before installation",
                self.server_name
            )));
        }

        *self.imported_tools.write_recover() = import_tools(&self.server_name, tools);
        let peer = service.peer().clone();
        let request_tasks = service.service().request_tasks();
        let cancellation = service.cancellation_token();
        let quit = Arc::new(ServiceQuit::default());
        let previous_service = {
            let mut service_guard = self.service.lock().await;
            self.service_generation
                .store(next_generation, Ordering::SeqCst);
            self.connected.store(true, Ordering::SeqCst);
            service_guard.replace(McpService {
                peer,
                request_tasks: Arc::clone(&request_tasks),
                cancellation: Some(cancellation),
                quit: Arc::clone(&quit),
                stdio_child: stdio_child.clone(),
            })
        };
        // A fresh connection starts with a fresh timeout budget.
        self.consecutive_timeouts.store(0, Ordering::SeqCst);
        *self.last_error.write_recover() = None;
        self.reconnect_exhausted.store(false, Ordering::SeqCst);
        self.spawn_service_watcher(
            service,
            next_generation,
            request_tasks,
            Arc::clone(&quit),
            stdio_child,
        );
        if let Some(previous_service) = previous_service {
            stop_service(previous_service).await;
        }
        Ok(())
    }

    fn spawn_service_watcher(
        self: &Arc<Self>,
        service: RunningService<RoleClient, LashMcpClientHandler>,
        service_generation: u64,
        request_tasks: Arc<McpHostRequestTasks>,
        quit: Arc<ServiceQuit>,
        stdio_child: Option<Arc<StdioChildGuard>>,
    ) {
        let weak_entry = Arc::downgrade(self);
        tokio::spawn(async move {
            let waiting = service.waiting();
            tokio::pin!(waiting);
            let healthy_dwell = tokio::time::sleep(
                weak_entry
                    .upgrade()
                    .map_or(Duration::ZERO, |entry| entry.reconnect_healthy_dwell()),
            );
            tokio::pin!(healthy_dwell);
            let reason = tokio::select! {
                reason = &mut waiting => reason,
                () = &mut healthy_dwell => {
                    if let Some(entry) = weak_entry.upgrade() {
                        entry
                            .reset_reconnect_backoff_if_current(service_generation)
                            .await;
                    }
                    waiting.await
                }
            };
            if let Some(child) = &stdio_child {
                child.reap_after_ungraceful_drop();
            }
            request_tasks.shutdown().await;
            if let Some(entry) = weak_entry.upgrade() {
                entry.observe_service_quit(service_generation, reason).await;
            }
            quit.finish();
        });
    }

    async fn observe_service_quit(
        self: &Arc<Self>,
        observed_generation: u64,
        reason: Result<QuitReason, tokio::task::JoinError>,
    ) {
        let disconnected = {
            let mut guard = self.service.lock().await;
            if self.service_generation.load(Ordering::SeqCst) != observed_generation
                || guard.is_none()
            {
                false
            } else {
                self.connected.store(false, Ordering::SeqCst);
                *self.last_error.write_recover() = Some(format!(
                    "MCP server `{}` service quit: {reason:?}",
                    self.server_name
                ));
                guard.take();
                true
            }
        };
        if disconnected && !self.cancelled.load(Ordering::SeqCst) {
            self.spawn_reconnect_loop();
        }
    }

    async fn refresh_tools(&self, peer: Peer<RoleClient>, observed_generation: u64) {
        let discovery_timeout = self.config.startup_timeout();
        let tools = match timeout(discovery_timeout, peer.list_all_tools()).await {
            Ok(Ok(tools)) => tools,
            Ok(Err(error)) => {
                tracing::warn!(
                    server = %self.server_name,
                    error = %error,
                    "MCP tools/list refresh failed after list-changed notification"
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    server = %self.server_name,
                    timeout_ms = discovery_timeout.as_millis() as u64,
                    "MCP tools/list refresh timed out after list-changed notification"
                );
                return;
            }
        };
        let service_guard = self.service.lock().await;
        if self.cancelled.load(Ordering::SeqCst)
            || service_guard.is_none()
            || self.service_generation.load(Ordering::SeqCst) != observed_generation
        {
            return;
        }
        *self.imported_tools.write_recover() = import_tools(&self.server_name, tools);
    }

    /// Retry [`establish`](Self::establish) with exponential backoff until it
    /// succeeds or the entry is detached. At most one loop runs per entry.
    fn spawn_reconnect_loop(self: &Arc<Self>) {
        if self.cancelled.load(Ordering::SeqCst) || self.connecting.swap(true, Ordering::SeqCst) {
            return;
        }
        self.reconnect_exhausted.store(false, Ordering::SeqCst);
        let entry = Arc::clone(self);
        tokio::spawn(async move {
            let max_attempts = entry.config.reconnect_max_attempts();
            let mut attempts = 0_u64;
            let mut published_service = false;
            loop {
                let cancelled = entry.cancelled_notify.notified();
                if entry.cancelled.load(Ordering::SeqCst) {
                    break;
                }
                let backoff = entry.reconnect_backoff();
                tokio::select! {
                    () = tokio::time::sleep((entry.reconnect_jitter)(backoff)) => {}
                    () = cancelled => {}
                }
                if entry.cancelled.load(Ordering::SeqCst) {
                    break;
                }
                attempts += 1;
                entry.advance_reconnect_backoff();
                match entry.establish().await {
                    Ok(()) => {
                        published_service = true;
                        tracing::info!(server = %entry.server_name, "MCP server reconnected");
                        #[cfg(test)]
                        entry.pause_after_reconnect_publish().await;
                        break;
                    }
                    Err(err) => {
                        if entry.cancelled.load(Ordering::SeqCst) {
                            break;
                        }
                        tracing::warn!(
                            server = %entry.server_name,
                            error = %err,
                            "MCP reconnect attempt failed"
                        );
                        if max_attempts != 0 && attempts >= max_attempts {
                            *entry.last_error.write_recover() = Some(format!(
                                "MCP server `{}` reconnect attempts exhausted after {attempts} attempt(s); no background recovery is active; last error: {err}",
                                entry.server_name
                            ));
                            entry.reconnect_exhausted.store(true, Ordering::SeqCst);
                            tracing::warn!(
                                server = %entry.server_name,
                                attempts,
                                "MCP reconnect attempts exhausted"
                            );
                            break;
                        }
                    }
                }
            }
            entry.connecting.store(false, Ordering::SeqCst);
            entry.reconnect_idle_notify.notify_waiters();
            if published_service
                && !entry.connected.load(Ordering::SeqCst)
                && !entry.cancelled.load(Ordering::SeqCst)
            {
                entry.spawn_reconnect_loop();
            }
        });
    }

    /// Drop the dead service and start reconnecting in the background. The
    /// imported tool definitions are kept so the tool catalog stays stable.
    async fn mark_disconnected(self: &Arc<Self>, cause: String, observed_generation: u64) -> bool {
        let service = {
            let mut guard = self.service.lock().await;
            if self.service_generation.load(Ordering::SeqCst) != observed_generation
                || guard.is_none()
            {
                return false;
            }
            self.connected.store(false, Ordering::SeqCst);
            *self.last_error.write_recover() = Some(cause);
            guard.take()
        };
        if let Some(service) = service {
            stop_service(service).await;
        }
        if !self.cancelled.load(Ordering::SeqCst) {
            self.spawn_reconnect_loop();
        }
        true
    }

    async fn handle_call_timeout(
        self: &Arc<Self>,
        peer: &Peer<RoleClient>,
        observed_generation: u64,
        expired_timeout: Duration,
    ) -> ToolResult {
        let server_name = self.server_name.clone();
        let timeout_failure = |code| {
            ToolResult::retryable_failure(
                ToolFailureClass::Timeout,
                code,
                McpError::CallTimeout {
                    server: server_name.clone(),
                    timeout_ms: expired_timeout.as_millis() as u64,
                }
                .to_string(),
                None,
            )
        };

        // rmcp reports which configured clock expired. Validation requires the
        // wall cap to be strictly greater than the idle duration, so the two
        // expiries are unambiguous. Unknown future timeout sources take the
        // conservative wall-cap path and never affect connection health.
        match expired_timeout {
            timeout if timeout == self.config.call_max_total_timeout() => {
                return timeout_failure("mcp_call_deadline_exceeded");
            }
            timeout if timeout == self.config.call_timeout() => {}
            _ => return timeout_failure("mcp_call_deadline_exceeded"),
        }

        let timeout_failure = || timeout_failure("mcp_call_timeout");

        match self.effective_timeout_disconnect_policy(peer) {
            TimeoutDisconnectPolicy::Never => timeout_failure(),
            TimeoutDisconnectPolicy::PingProbe => match self.probe_peer(peer).await {
                Ok(()) => timeout_failure(),
                Err(err) => {
                    let cause = format!(
                        "MCP server `{server_name}` failed liveness probe after a call timeout: {err}"
                    );
                    if !self
                        .mark_disconnected(cause.clone(), observed_generation)
                        .await
                    {
                        return timeout_failure();
                    }
                    ToolResult::retryable_failure(
                        ToolFailureClass::Unavailable,
                        "mcp_connection_lost",
                        format!("{cause}; reconnecting in the background"),
                        Some(self.config.reconnect_initial_backoff().as_millis() as u64),
                    )
                }
            },
            TimeoutDisconnectPolicy::ConsecutiveTimeouts => {
                let consecutive = {
                    let guard = self.service.lock().await;
                    if self.service_generation.load(Ordering::SeqCst) != observed_generation
                        || guard.is_none()
                    {
                        return timeout_failure();
                    }
                    self.consecutive_timeouts.fetch_add(1, Ordering::SeqCst) + 1
                };
                if consecutive < self.config.consecutive_timeouts_before_disconnect() {
                    return timeout_failure();
                }
                let cause = format!(
                    "MCP server `{server_name}` reached {consecutive} consecutive call timeouts"
                );
                if !self
                    .mark_disconnected(cause.clone(), observed_generation)
                    .await
                {
                    return timeout_failure();
                }
                ToolResult::retryable_failure(
                    ToolFailureClass::Unavailable,
                    "mcp_connection_lost",
                    format!("{cause}; reconnecting in the background"),
                    Some(self.config.reconnect_initial_backoff().as_millis() as u64),
                )
            }
        }
    }

    fn effective_timeout_disconnect_policy(
        &self,
        peer: &Peer<RoleClient>,
    ) -> TimeoutDisconnectPolicy {
        let configured = self.config.timeout_disconnect_policy();
        if configured == TimeoutDisconnectPolicy::PingProbe && !self.peer_supports_ping(peer) {
            TimeoutDisconnectPolicy::ConsecutiveTimeouts
        } else {
            configured
        }
    }

    fn peer_supports_ping(&self, peer: &Peer<RoleClient>) -> bool {
        let ping_supported = peer.peer_info().is_none_or(|info| {
            info.protocol_version.as_str() < ProtocolVersion::V_2026_07_28.as_str()
        });
        if !ping_supported && !self.ping_degrade_warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                server = %self.server_name,
                protocol_version = %peer.peer_info().expect("peer info checked").protocol_version,
                "MCP ping is unavailable for the negotiated protocol; degrading timeout policy to consecutive_timeouts and disabling interval probes"
            );
        }
        ping_supported
    }

    async fn probe_peer(&self, peer: &Peer<RoleClient>) -> Result<(), ServiceError> {
        let probe_timeout = self.config.liveness_probe_timeout();
        match timeout(
            probe_timeout,
            peer.send_request(ClientRequest::PingRequest(PingRequest::default())),
        )
        .await
        {
            // Any well-formed answer proves the transport and server loop are
            // alive. This includes unexpected success shapes and JSON-RPC
            // errors such as -32601 from a server without `ping`.
            Ok(Ok(_)) | Ok(Err(ServiceError::McpError(_))) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ServiceError::Timeout {
                timeout: probe_timeout,
            }),
        }
    }

    fn spawn_keepalive_loop(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let interval = self.config.liveness_probe_interval();
        if interval.is_zero() || self.keepalive_started.swap(true, Ordering::SeqCst) {
            return None;
        }
        let weak_entry = Arc::downgrade(self);
        let cancelled_notify = Arc::clone(&self.cancelled_notify);
        Some(tokio::spawn(async move {
            loop {
                let cancelled = cancelled_notify.notified();
                let Some(entry) = weak_entry.upgrade() else {
                    return;
                };
                if entry.cancelled.load(Ordering::SeqCst) {
                    break;
                }
                drop(entry);
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = cancelled => {}
                }
                let Some(entry) = weak_entry.upgrade() else {
                    return;
                };
                if entry.cancelled.load(Ordering::SeqCst) {
                    break;
                }
                let service = entry.service.lock().await;
                let peer = service.as_ref().map(|service| {
                    (
                        service.peer().clone(),
                        entry.service_generation.load(Ordering::SeqCst),
                    )
                });
                drop(service);
                let Some((peer, service_generation)) = peer else {
                    if !entry.connecting.load(Ordering::SeqCst) {
                        entry.spawn_reconnect_loop();
                    }
                    continue;
                };
                if !entry.peer_supports_ping(&peer) {
                    break;
                }
                let failure = if peer.is_transport_closed() {
                    Some("transport closed before liveness probe".to_string())
                } else {
                    entry
                        .probe_peer(&peer)
                        .await
                        .err()
                        .map(|err| err.to_string())
                };
                if let Some(failure) = failure {
                    entry
                        .mark_disconnected(
                            format!(
                                "MCP server `{}` background liveness probe failed: {failure}",
                                entry.server_name
                            ),
                            service_generation,
                        )
                        .await;
                }
            }
            if let Some(entry) = weak_entry.upgrade() {
                entry.keepalive_started.store(false, Ordering::SeqCst);
                entry.reconnect_idle_notify.notify_waiters();
            }
        }))
    }

    async fn record_call_success(&self, observed_generation: u64) {
        let guard = self.service.lock().await;
        if self.service_generation.load(Ordering::SeqCst) != observed_generation || guard.is_none()
        {
            return;
        }
        self.consecutive_timeouts.store(0, Ordering::SeqCst);
        *self.last_error.write_recover() = None;
    }

    async fn shutdown(&self) {
        self.connected.store(false, Ordering::SeqCst);
        self.cancel_service().await;
        loop {
            let idle = self.reconnect_idle_notify.notified();
            if !self.connecting.load(Ordering::SeqCst)
                && !self.keepalive_started.load(Ordering::SeqCst)
            {
                break;
            }
            idle.await;
        }
        // Cover the race where a reconnect passed its cancellation check just
        // before shutdown set the flag and installed while teardown waited.
        self.cancel_service().await;
    }

    async fn cancel_service(&self) {
        let service = self.service.lock().await.take();
        if let Some(service) = service {
            stop_service(service).await;
        }
    }
}

fn import_tools(
    server_name: &str,
    mut tools: Vec<rmcp::model::Tool>,
) -> BTreeMap<String, ImportedTool> {
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let mut used_names = BTreeSet::new();
    let mut imported = BTreeMap::new();
    for tool in tools {
        let original_name = tool.name.to_string();
        let description = tool
            .description
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        let input_schema = Value::Object((*tool.input_schema).clone());
        let output_schema = tool
            .output_schema
            .as_ref()
            .map(|s| Value::Object((**s).clone()))
            .unwrap_or_else(|| json!({}));
        let (prefixed, lashlang_binding) =
            naming::build_prefixed_name(server_name, &original_name, &mut used_names);

        let description = if description.is_empty() {
            format!("MCP tool from server `{server_name}`")
        } else {
            format!("[MCP {server_name}] {description}")
        };

        imported.insert(
            prefixed.clone(),
            ImportedTool {
                original_name,
                definition: ToolDefinition::raw(
                    format!("mcp:{server_name}/{prefixed}"),
                    prefixed,
                    description,
                    input_schema,
                    output_schema,
                )
                .with_lashlang_binding(lashlang_binding),
            },
        );
    }
    imported
}

async fn tool_result_from_rmcp(
    result: rmcp::model::CallToolResult,
    context: &ToolContext<'_>,
    binary_content_attachments: bool,
) -> ToolResult {
    let is_error = result.is_error.unwrap_or(false);

    let mut text_parts = Vec::new();
    let mut content_items: Vec<ToolValue> = Vec::new();
    let mut has_attachments = false;

    for Content { raw, .. } in result.content {
        match raw {
            RawContent::Text(text) => {
                text_parts.push(text.text.clone());
                content_items.push(ToolValue::String(text.text));
            }
            RawContent::Image(image) => {
                let reference =
                    match store_mcp_attachment(context, &image.data, &image.mime_type, "MCP image")
                        .await
                    {
                        Ok(reference) => reference,
                        Err(result) => return result,
                    };
                has_attachments = true;
                content_items.push(ToolValue::Attachment(lash_core::AttachmentSource::stored(
                    reference,
                )));
            }
            RawContent::Audio(audio) if binary_content_attachments => {
                let reference =
                    match store_mcp_attachment(context, &audio.data, &audio.mime_type, "MCP audio")
                        .await
                    {
                        Ok(reference) => reference,
                        Err(result) => return result,
                    };
                has_attachments = true;
                content_items.push(ToolValue::Attachment(lash_core::AttachmentSource::stored(
                    reference,
                )));
            }
            RawContent::Resource(resource) if binary_content_attachments => {
                match resource.resource {
                    ResourceContents::BlobResourceContents {
                        uri,
                        mime_type,
                        blob,
                        ..
                    } => {
                        let Some(mime_type) = mime_type else {
                            return ToolResult::err_fmt(
                                "MCP binary resource attachment is missing its MIME type",
                            );
                        };
                        let reference = match store_mcp_attachment(
                            context,
                            &blob,
                            &mime_type,
                            &format!("MCP resource {uri}"),
                        )
                        .await
                        {
                            Ok(reference) => reference,
                            Err(result) => return result,
                        };
                        has_attachments = true;
                        content_items.push(ToolValue::Attachment(
                            lash_core::AttachmentSource::stored(reference),
                        ));
                    }
                    text_resource => {
                        if let Ok(value) = serde_json::to_value(text_resource) {
                            content_items.push(ToolValue::from(value));
                        }
                    }
                }
            }
            other => {
                if let Ok(value) = serde_json::to_value(&other) {
                    content_items.push(ToolValue::from(value));
                }
            }
        }
    }

    let value = if let Some(structured) = result.structured_content {
        if !has_attachments {
            ToolValue::from(structured)
        } else {
            ToolValue::Object(
                [
                    ("structured".to_string(), ToolValue::from(structured)),
                    ("content".to_string(), ToolValue::Array(content_items)),
                ]
                .into_iter()
                .collect(),
            )
        }
    } else if content_items.is_empty() {
        ToolValue::Null
    } else if content_items.len() == 1 {
        content_items.into_iter().next().unwrap_or(ToolValue::Null)
    } else {
        ToolValue::Array(content_items)
    };
    if is_error {
        ToolResult::from_output(ToolCallOutput::failure(ToolFailure {
            class: ToolFailureClass::Execution,
            code: "mcp_tool_error".into(),
            message: if text_parts.is_empty() {
                "MCP tool returned an error".into()
            } else {
                text_parts.join("\n\n")
            },
            source: ToolFailureSource::Tool,
            retry: ToolRetryDisposition::Never,
            raw: Some(value),
        }))
    } else {
        ToolResult::from_output(ToolCallOutput::success(value))
    }
}

async fn store_mcp_attachment(
    context: &ToolContext<'_>,
    encoded: &str,
    mime_type: &str,
    label: &str,
) -> Result<lash_core::AttachmentRef, ToolResult> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| ToolResult::err_fmt(McpError::Decode(err)))?;
    let media_type = MediaType::parse(mime_type)
        .map_err(|err| ToolResult::err_fmt(format_args!("Invalid MCP attachment MIME: {err}")))?;
    context
        .attachments()
        .put(
            data,
            AttachmentCreateMeta::new(media_type, None, Some(label.to_string())),
        )
        .await
        .map_err(|err| ToolResult::err_fmt(format_args!("Failed to store MCP attachment: {err}")))
}

impl Drop for McpConnectionPool {
    fn drop(&mut self) {
        for entry in self.entries.get_mut().recover().values() {
            entry.cancel();
        }
        // We can't .await in Drop. Entry drop cancels every rmcp service; stdio
        // entries additionally retain their exact child-process handle so
        // teardown can kill and wait without relying on the dropped runtime.
        // For graceful shutdown, callers should call `shutdown_all` themselves.
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod policy_tests;

#[cfg(test)]
#[path = "pool_unit_tests.rs"]
mod tests;
