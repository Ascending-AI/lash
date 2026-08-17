//! Per-core MCP connection pool.
//!
//! [`McpConnectionPool`] holds one client per configured server and is shared
//! across every session built from the same [`lash_core::LashCore`]. The pool
//! attempts to connect each server eagerly when constructed, but a server
//! that is down never fails construction: the entry stays registered and a
//! entry-owned lifecycle actor retries with exponential backoff until it
//! connects (or the server is detached). The same actor observes a connection
//! that dies mid-life and re-establishes it. Imported tool
//! definitions are kept across a disconnect so the tool catalog stays stable;
//! calls to a disconnected server fail loudly instead.
//!
//! The wire-level transport is provided by the official [`rmcp`] SDK.

mod lifecycle_actor;

use lash_sansio::sync::{LockResultExt, MutexExt, RwLockExt};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
use rmcp::service::{Peer, PeerRequestOptions, RoleClient};
use serde_json::{Value, json};
use tokio::time::timeout;

use lash_core::{
    AttachmentCreateMeta, AttemptContext, MediaType, ToolCallOutput, ToolDefinition, ToolFailure,
    ToolFailureClass, ToolFailureSource, ToolResult, ToolRetryDisposition, ToolValue,
};
use lash_tool_support::ToolDefinitionLashlangExt;

#[cfg(test)]
use crate::config::McpCallPolicy;
use crate::config::{McpServerConfig, TimeoutDisconnectPolicy};
use crate::error::McpError;
use crate::host::{McpHostServices, McpToolListChangedHandler};
use crate::naming;
#[cfg(test)]
use crate::service_lifecycle::build_http_headers;
use crate::service_lifecycle::{equal_jitter, is_connection_loss};
use lifecycle_actor::{LifecycleActor, LifecycleCommand};

/// One entry's complete explicit-shutdown budget. Every actor await that can
/// precede cleanup is preempted by `Shutdown`, leaving the full three-second
/// graceful close plus one-second post-kill reap and one second of scheduling
/// margin: `3s + 1s + 1s = 5s`.
///
/// All entry actors are joined concurrently, so `shutdown_all()` takes roughly
/// one five-second bound rather than `entries * five seconds`.
const ENTRY_SHUTDOWN_TOTAL_BOUND: Duration = Duration::from_secs(5);

/// Shared, per-core connection pool. Wrapped in `Arc` and cloned into each
/// session plugin instance.
///
/// Hosts must call [`McpConnectionPool::shutdown_all`] before dropping their
/// last pool handle to reclaim stdio children within a bounded deadline.
/// Dropping a live pool only sends each child a best-effort kill and logs an
/// error; it does not wait, so the child remains a zombie until the host
/// process exits.
/// Cancelling `shutdown_all()` mid-flight aborts the actor and likewise leaves
/// any killed stdio child unreaped.
pub struct McpConnectionPool {
    entries: RwLock<BTreeMap<String, Arc<McpEntry>>>,
    host_services: McpHostServices,
    shut_down: AtomicBool,
    #[cfg(test)]
    mid_establish_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
    #[cfg(test)]
    attach_return_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
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
    /// Read-only publication cell. The actor alone owns the service and writes
    /// this peer/generation snapshot; dispatch never routes through the actor.
    service: tokio::sync::watch::Receiver<Option<Arc<PublishedService>>>,
    actor_tx: tokio::sync::mpsc::UnboundedSender<LifecycleCommand>,
    actor_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    active_pid: Arc<AtomicU32>,
    /// Cached, prefixed tool definitions for this server, refreshed on every
    /// successful (re)connect and kept across a disconnect so the tool
    /// surface stays stable. Keys are the prefixed names
    /// (`mcp__<server>__<tool>`).
    imported_tools: RwLock<BTreeMap<String, ImportedTool>>,
    last_error: RwLock<Option<String>>,
    shutting_down: Arc<AtomicBool>,
    /// Applies randomized delay to the entry-owned reconnect ceiling. Kept as
    /// a seam so pacing tests can observe ceilings without wall-clock sleeps.
    reconnect_jitter: RwLock<Arc<dyn Fn(Duration) -> Duration + Send + Sync>>,
    /// Set when a bounded reconnect loop spends its final attempt.
    reconnect_exhausted: AtomicBool,
    /// Consecutive idle timeouts since the last successful tool call. Both
    /// increments and resets are generation-stamped messages, so accounting
    /// is asynchronously serialized by the lifecycle actor.
    consecutive_timeouts: AtomicU64,
    /// Keeps the protocol-version degradation warning to once per server.
    ping_degrade_warned: AtomicBool,
    #[cfg(test)]
    mid_establish_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
    #[cfg(test)]
    refresh_install_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
    #[cfg(test)]
    panic_actor_on_quit: AtomicBool,
    #[cfg(test)]
    shutdown_wedge_pid: AtomicU32,
    #[cfg(test)]
    never_finish_child_reap: AtomicBool,
}

#[derive(Clone)]
struct PublishedService {
    peer: Peer<RoleClient>,
    generation: u64,
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
            #[cfg(test)]
            mid_establish_hook: RwLock::new(None),
            #[cfg(test)]
            attach_return_hook: RwLock::new(None),
        }
    }

    /// Build a pool for the configured servers. Every server is tried eagerly
    /// in parallel so tools are available immediately when servers are up, but a
    /// connection failure never aborts construction: the entry stays
    /// registered and reconnects in the background. Only configuration errors
    /// (a misconfigured server, not an outage) fail the build. The host must
    /// call [`McpConnectionPool::shutdown_all`] to fully reap stdio children;
    /// dropping the returned pool kills but deliberately does not wait.
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
            let entry = McpEntry::new(name.clone(), config, pool.host_services.clone());
            if let Err((rejected, error)) = pool.install(name.clone(), Arc::clone(&entry)) {
                rejected.shutdown().await;
                return Err(error);
            }
            entries.push((name, entry));
        }
        join_all(entries.into_iter().map(|(name, entry)| async move {
            let connect_result = entry.establish().await;
            if let Err(err) = connect_result {
                tracing::warn!(
                    server = %name,
                    error = %err,
                    "MCP server unavailable at startup; retrying in the background"
                );
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
            return Err(McpError::PoolShutDown);
        }
        config.validate(&server_name)?;
        self.validate_server_prefix_available(&server_name)?;
        let entry = McpEntry::new(server_name.clone(), config, self.host_services.clone());
        #[cfg(test)]
        entry.set_mid_establish_hook(self.mid_establish_hook.read_recover().clone());
        let previous = match self.install(server_name.clone(), Arc::clone(&entry)) {
            Ok(previous) => previous,
            Err((rejected, error)) => {
                rejected.shutdown().await;
                return Err(error);
            }
        };
        if let Some(previous) = previous {
            previous.shutdown().await;
        }
        let connect_result = entry.establish().await;
        #[cfg(test)]
        let attach_return_hook = self.attach_return_hook.read_recover().clone();
        #[cfg(test)]
        if let Some(hook) = attach_return_hook {
            hook.reached.notify_one();
            hook.release.notified().await;
        }
        if self.shut_down.load(Ordering::SeqCst) || entry.shutting_down.load(Ordering::SeqCst) {
            return Err(McpError::PoolShutDown);
        }
        if let Err(err) = connect_result {
            if matches!(err, McpError::PoolShutDown) {
                return Err(err);
            }
            tracing::warn!(
                server = %server_name,
                error = %err,
                "MCP server unavailable during attach; retrying in the background"
            );
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
                return Err((entry, McpError::PoolShutDown));
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
                connected: entry.service_snapshot().is_some(),
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
            .filter(|entry| entry.service_snapshot().is_some())
            .cloned()
            .collect();
        let mut peers = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(service) = entry.service_snapshot() {
                peers.push((entry.server_name.clone(), service.peer.clone()));
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
        context: &AttemptContext<'_>,
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

        // The actor publishes a peer/generation snapshot through `watch`.
        // Dispatch clones that cheap handle without awaiting or routing calls
        // through lifecycle coordination.
        let (peer, service_generation) = {
            match entry.service_snapshot() {
                Some(service) => (service.peer.clone(), service.generation),
                None => {
                    if entry.shutting_down.load(Ordering::SeqCst) {
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
            entry.mark_disconnected(cause.clone(), service_generation);
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
                entry.record_call_success(service_generation);
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
                    entry.mark_disconnected(cause.clone(), service_generation);
                    if entry.shutting_down.load(Ordering::SeqCst) {
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
    /// pool for a graceful shutdown; `Drop` itself cannot await. Every actor is
    /// sent `Shutdown` before the handles are joined concurrently. One literal
    /// five-second per-entry total deadline covers graceful close, kill, and
    /// bounded reap, so total pool shutdown is approximately five seconds, not
    /// the number of entries multiplied by five seconds.
    ///
    /// A child can be abandoned if it survives the actor's preemptive kill and
    /// bounded reap or if the entry deadline expires mid-reap. The deadline
    /// abort branch reports the live `active_pid`; its literal PID and reason
    /// are recorded in `last_error` and tracing. No background waitpid sweep is
    /// retained.
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
    fn new(
        server_name: String,
        config: McpServerConfig,
        host_services: McpHostServices,
    ) -> Arc<Self> {
        let (actor_tx, actor_rx) = tokio::sync::mpsc::unbounded_channel();
        let (published_tx, service) = tokio::sync::watch::channel(None);
        let active_pid = Arc::new(AtomicU32::new(0));
        let reconnect_initial_backoff = config.reconnect_initial_backoff();
        let keepalive_interval = config.liveness_probe_interval();
        Arc::new_cyclic(|weak| {
            let actor = LifecycleActor::new(
                weak.clone(),
                actor_rx,
                published_tx,
                Arc::clone(&active_pid),
                reconnect_initial_backoff,
                keepalive_interval,
            );
            let actor_handle = tokio::spawn(actor.run());
            Self {
                server_name,
                config,
                host_services,
                service,
                actor_tx,
                actor_handle: Mutex::new(Some(actor_handle)),
                active_pid,
                imported_tools: RwLock::new(BTreeMap::new()),
                last_error: RwLock::new(None),
                shutting_down: Arc::new(AtomicBool::new(false)),
                reconnect_jitter: RwLock::new(Arc::new(equal_jitter)),
                reconnect_exhausted: AtomicBool::new(false),
                consecutive_timeouts: AtomicU64::new(0),
                ping_degrade_warned: AtomicBool::new(false),
                #[cfg(test)]
                mid_establish_hook: RwLock::new(None),
                #[cfg(test)]
                refresh_install_hook: RwLock::new(None),
                #[cfg(test)]
                panic_actor_on_quit: AtomicBool::new(false),
                #[cfg(test)]
                shutdown_wedge_pid: AtomicU32::new(0),
                #[cfg(test)]
                never_finish_child_reap: AtomicBool::new(false),
            }
        })
    }

    fn service_snapshot(&self) -> Option<Arc<PublishedService>> {
        self.service.borrow().clone()
    }

    async fn establish(&self) -> Result<(), McpError> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(McpError::PoolShutDown);
        }
        let (reply, result) = tokio::sync::oneshot::channel();
        self.actor_tx
            .send(LifecycleCommand::Establish { reply })
            .map_err(|_| McpError::PoolShutDown)?;
        result.await.unwrap_or(Err(McpError::PoolShutDown))
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
        #[cfg(test)]
        self.pause_before_refresh_install().await;
        let _ = self.actor_tx.send(LifecycleCommand::InstallToolCatalog {
            generation: observed_generation,
            tools,
        });
    }

    fn mark_disconnected(&self, cause: String, observed_generation: u64) -> bool {
        if self
            .service_snapshot()
            .as_ref()
            .map(|service| service.generation)
            != Some(observed_generation)
        {
            return false;
        }
        self.actor_tx
            .send(LifecycleCommand::Disconnect {
                generation: observed_generation,
                cause,
            })
            .is_ok()
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
                    if !self.mark_disconnected(cause.clone(), observed_generation) {
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
                let (reply, result) = tokio::sync::oneshot::channel();
                if self
                    .actor_tx
                    .send(LifecycleCommand::CallTimedOut {
                        generation: observed_generation,
                        reply,
                    })
                    .is_err()
                {
                    return timeout_failure();
                }
                let Ok(Some(cause)) = result.await else {
                    return timeout_failure();
                };
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

    fn record_call_success(&self, observed_generation: u64) {
        let _ = self.actor_tx.send(LifecycleCommand::CallSucceeded {
            generation: observed_generation,
        });
    }

    async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let _ = self.actor_tx.send(LifecycleCommand::Shutdown);
        let handle = self.actor_handle.lock_recover().take();
        let Some(mut handle) = handle else {
            return;
        };
        let mut abort_on_drop = AbortOnDrop::new(handle.abort_handle());
        match timeout(ENTRY_SHUTDOWN_TOTAL_BOUND, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let reason = format!("MCP lifecycle actor terminated with JoinError: {error}");
                *self.last_error.write_recover() = Some(reason.clone());
                tracing::error!(server = %self.server_name, reason = %reason, "MCP lifecycle actor failed during explicit shutdown");
            }
            Err(_) => {
                let pid = self.active_pid.load(Ordering::SeqCst);
                handle.abort();
                let _ = handle.await;
                let reason = if pid == 0 {
                    "MCP lifecycle actor abandoned: it did not finish within the 5s per-entry total shutdown deadline".to_string()
                } else {
                    format!(
                        "MCP stdio child PID {pid} abandoned: lifecycle actor did not finish within the 5s per-entry total shutdown deadline"
                    )
                };
                *self.last_error.write_recover() = Some(reason.clone());
                tracing::error!(
                    server = %self.server_name,
                    pid = (pid != 0).then_some(pid),
                    reason = %reason,
                    "MCP explicit shutdown abandoned a wedged lifecycle actor"
                );
            }
        }
        abort_on_drop.disarm();
    }

    #[cfg(test)]
    fn set_mid_establish_hook(&self, hook: Option<Arc<policy_tests::ActorPauseHook>>) {
        *self.mid_establish_hook.write_recover() = hook;
    }

    #[cfg(test)]
    async fn pause_before_refresh_install(&self) {
        let hook = self.refresh_install_hook.read_recover().clone();
        if let Some(hook) = hook {
            hook.reached.notify_one();
            hook.release.notified().await;
        }
    }
}

struct AbortOnDrop {
    handle: tokio::task::AbortHandle,
    armed: bool,
}

impl AbortOnDrop {
    fn new(handle: tokio::task::AbortHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
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
    context: &AttemptContext<'_>,
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
    context: &AttemptContext<'_>,
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
        // We cannot await in `Drop`. Dropping the entries closes each actor's
        // command channel; actor-owned stdio guards then kill before logging
        // and deliberately never wait. Hosts that need bounded reap attempts
        // must call `shutdown_all` first.
    }
}

impl Drop for McpEntry {
    fn drop(&mut self) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        if let Some(handle) = self.actor_handle.get_mut().recover().take() {
            handle.abort();
        }
        let pid = self.active_pid.load(Ordering::SeqCst);
        if pid != 0 {
            tracing::error!(
                pid,
                server = %self.server_name,
                "MCP stdio child killed without explicit pool shutdown; call shutdown_all() to reap it"
            );
        }
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod policy_tests;

#[cfg(test)]
#[path = "pool_unit_tests.rs"]
mod tests;
