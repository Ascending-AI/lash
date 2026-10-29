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

mod catalog;
mod result_schema;
use result_schema::mcp_result_schema;

#[cfg(test)]
pub(crate) fn result_contract_fixture() -> lash_vocabulary::ToolContract {
    ToolDefinition::raw(
        "fixture",
        "mcp.fixture",
        "MCP result fixture",
        serde_json::json!({}),
        mcp_result_schema(None),
    )
    .expect("the MCP result schema is valid")
    .with_execution(std::time::Duration::from_secs(30))
    .contract()
}

mod admission;
mod attempt;
pub(crate) mod guidance;
mod lifecycle_actor;
mod round_trip;
pub(crate) mod tool_changes;
use admission::RemoteCompletion;
#[cfg(test)]
pub(crate) use admission::linked_server;

use lash_protocol_types::{sync::LockResultExt, sync::MutexExt, sync::RwLockExt};

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine;
use futures_util::future::join_all;
#[cfg(test)]
use http::HeaderName;
use rmcp::ServiceError;
use rmcp::model::{
    CallToolRequestParams, ClientRequest, ContentBlock, Request, ResourceContents, Role,
    ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use tokio::time::timeout;

use lash_core::AttachmentCreateMeta;
use lash_core::AttemptContext;
use lash_core::MediaType;
use lash_core::ToolCallOutput;
use lash_core::ToolOutcome;
use lash_core::ToolValue;
use lash_core::ToolView;
use lash_core::ToolViewBlock;
use lash_core::ToolViewMeta;
use lash_vocabulary::tool_contract::ToolDefinition;
use lash_vocabulary::tool_contract::ToolId;

use crate::call_failure::{McpCallFailure, McpServiceFailure};
#[cfg(test)]
use crate::config::McpCallPolicy;
use crate::config::{McpServerConfig, TimeoutDisconnectPolicy};
#[cfg(test)]
use crate::config::{McpStdioTransport, McpTransport};
use crate::error::McpError;
use crate::host::McpHostServices;
use crate::naming;
#[cfg(test)]
use crate::service_lifecycle::build_http_headers;
use crate::service_lifecycle::equal_jitter;
#[cfg(test)]
use lash_core::{ToolFailureClass, ToolFailureSource};
use lifecycle_actor::{LifecycleActor, LifecycleCommand};

/// A live connection an events request is sent on: its peer, and the
/// handler that routes its stream notifications.
pub(crate) type EventsPeer = (Peer<RoleClient>, crate::host::LashMcpClientHandler);

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
    /// The one owner of every installed entry's imported tools: advertisement,
    /// target resolution and catalog replacement all read and write it.
    catalog: Arc<RwLock<InstalledCatalog>>,
    host_services: McpHostServices,
    shut_down: AtomicBool,
    #[cfg(test)]
    mid_establish_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
    #[cfg(test)]
    attach_return_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
    #[cfg(test)]
    resolved_target_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
    #[cfg(test)]
    advertised_tools_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Copied onto every entry this pool installs, so tests observe the
    /// lifecycle of children spawned before they can reach the entry.
    #[cfg(test)]
    lifecycle_observer: RwLock<Option<crate::service_lifecycle::LifecycleObserver>>,
}

/// The diagnostic of an actor-owned health state, retaining schema admission causes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "cause", rename_all = "snake_case")]
pub enum McpServerFault {
    UnusableSchema(Box<lash_vocabulary::ToolCatalogBuildError>),
    Connection(String),
}

impl McpServerFault {
    pub fn message(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::UnusableSchema(source) => source.to_string().into(),
            Self::Connection(message) => message.into(),
        }
    }
}

impl std::fmt::Display for McpServerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Availability published by the entry's lifecycle actor. Diagnostics belong
/// to the state that produced them; dispatch only observes this value.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpServerHealth {
    Connecting,
    Connected {
        catalog_error: Option<McpServerFault>,
    },
    Reconnecting {
        last_error: Option<McpServerFault>,
    },
    /// Disconnected with automatic reconnect explicitly disabled.
    Disconnected {
        last_error: Option<McpServerFault>,
    },
    Exhausted {
        attempts: u64,
        last_error: Option<McpServerFault>,
    },
    ShuttingDown {
        reason: Option<McpServerFault>,
    },
}

impl McpServerHealth {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    pub fn is_shutting_down(&self) -> bool {
        matches!(self, Self::ShuttingDown { .. })
    }

    /// Typed diagnostic recorded by the actor for this state.
    pub fn fault(&self) -> Option<&McpServerFault> {
        match self {
            Self::Connecting => None,
            Self::Connected { catalog_error } => catalog_error.as_ref(),
            Self::Reconnecting { last_error }
            | Self::Disconnected { last_error }
            | Self::Exhausted { last_error, .. } => last_error.as_ref(),
            Self::ShuttingDown { reason } => reason.as_ref(),
        }
    }

    pub fn error(&self) -> Option<std::borrow::Cow<'_, str>> {
        self.fault().map(McpServerFault::message)
    }
}

/// Connection status of one configured server, for host/UI observability.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerStatus {
    pub server_name: String,
    pub tool_count: usize,
    pub health: McpServerHealth,
}

struct McpEntry {
    /// The pool's installed catalog, which this entry's actor replaces its
    /// own tools in while the entry is still the installed incarnation.
    catalog: Arc<RwLock<InstalledCatalog>>,
    server_name: String,
    config: McpServerConfig,
    host_services: McpHostServices,
    /// The actor alone owns the service and writes this peer/generation snapshot; dispatch
    /// never routes through the actor.
    service: tokio::sync::watch::Receiver<Option<Arc<PublishedService>>>,
    actor_tx: tokio::sync::mpsc::UnboundedSender<LifecycleCommand>,
    refresh_requested: tokio::sync::watch::Sender<u64>,
    actor_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    active_pid: Arc<AtomicU32>,
    health: Arc<RwLock<McpServerHealth>>,
    startup_failure: RwLock<Option<crate::error::McpStartupFailure>>,
    /// Kept as a seam so pacing tests can observe ceilings without wall-clock sleeps.
    reconnect_jitter: RwLock<Arc<dyn Fn(Duration) -> Duration + Send + Sync>>,
    /// Consecutive idle timeouts since the last successful tool call. Both
    /// increments and resets are generation-stamped messages, so accounting
    /// is asynchronously serialized by the lifecycle actor.
    consecutive_timeouts: AtomicU64,
    #[cfg(test)]
    mid_establish_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
    #[cfg(test)]
    refresh_install_hook: RwLock<Option<Arc<policy_tests::ActorPauseHook>>>,
    #[cfg(test)]
    refresh_notifications: AtomicU64,
    #[cfg(test)]
    panic_actor_on_quit: AtomicBool,
    #[cfg(test)]
    shutdown_wedge_pid: AtomicU32,
    #[cfg(test)]
    never_finish_child_reap: AtomicBool,
    /// Test-only rendezvous with the lifecycle actor and its child guard:
    /// deadlines armed, kills issued, children reaped or abandoned.
    #[cfg(test)]
    lifecycle_observer: RwLock<Option<crate::service_lifecycle::LifecycleObserver>>,
}

#[derive(Clone)]
struct PublishedService {
    peer: Peer<RoleClient>,
    generation: u64,
    handler: crate::host::LashMcpClientHandler,
}

struct ImportedTool {
    /// The native MCP tool name as advertised by the server (before
    /// prefixing/normalisation).
    original_name: String,
    definition: ToolDefinition,
    completion: RemoteCompletion,
}

/// Imported tools of every installed entry, keyed by configured server name.
/// Model-facing names are unique across the whole catalog.
type EnvironmentSubscriber = (
    lash_core_store::environment::SourceId,
    Weak<dyn Fn(lash_core_store::environment::EnvironmentChange) + Send + Sync>,
);

struct InstalledCatalog {
    servers: BTreeMap<String, InstalledServer>,
    revision: lash_core_store::environment::SourceRevision,
    subscribers: Vec<EnvironmentSubscriber>,
}
impl Default for InstalledCatalog {
    fn default() -> Self {
        Self {
            servers: BTreeMap::new(),
            revision: lash_core_store::environment::SourceRevision {
                incarnation: uuid::Uuid::new_v4(),
                counter: 0,
            },
            subscribers: Vec::new(),
        }
    }
}

struct InstalledServer {
    generation: u64,
    notifying: bool,
    /// The installed incarnation, the only entry whose actor may replace these
    /// tools. Weak because the entry owns the catalog.
    entry: Weak<McpEntry>,
    /// Prefixed tool definitions, replaced on every successful (re)connect or
    /// refresh and kept across a disconnect so the tool surface stays stable
    /// during an outage. Keys are the tools' operations, by effect name
    /// (`<server>.<tool>`), with symmetric `_h` eight-character identity
    /// suffixes on cleanup/truncation collision groups.
    tools: BTreeMap<String, ImportedTool>,
}

impl InstalledCatalog {
    fn changed(&mut self) {
        // One increment per accepted installation, under the catalog owner.
        if let Some(counter) = self.revision.counter.checked_add(1) {
            self.revision.counter = counter;
        } else {
            self.revision = lash_core_store::environment::SourceRevision {
                incarnation: uuid::Uuid::new_v4(),
                counter: 0,
            };
        }
        self.subscribers.retain(|(source, weak)| {
            if let Some(sink) = weak.upgrade() {
                sink(
                    lash_core_store::environment::EnvironmentChange::SourceRevision {
                        source: source.clone(),
                        revision: self.revision.clone(),
                    },
                );
                true
            } else {
                false
            }
        });
    }

    /// Installs `entry` as the current incarnation of its server with no tools
    /// yet; a replaced incarnation's tools leave with it.
    fn install(&mut self, entry: &Arc<McpEntry>) {
        self.servers.insert(
            entry.server_name.clone(),
            InstalledServer {
                entry: Arc::downgrade(entry),
                generation: 0,
                notifying: false,
                tools: BTreeMap::new(),
            },
        );
        self.changed();
    }

    fn retire(&mut self, entry: &McpEntry) {
        if self.is_current(entry) {
            self.servers.remove(&entry.server_name);
            self.changed();
        }
    }

    fn is_current(&self, entry: &McpEntry) -> bool {
        self.servers
            .get(&entry.server_name)
            .is_some_and(|installed| std::ptr::eq(installed.entry.as_ptr(), entry))
    }

    /// Replaces `entry`'s tools with `imported` as one publication, refusing a
    /// stale incarnation or any model-facing name another server owns.
    fn replace(
        &mut self,
        entry: &McpEntry,
        imported: BTreeMap<String, ImportedTool>,
        generation: u64,
        notifying: bool,
    ) -> Result<(), McpError> {
        if !self.is_current(entry)
            || self
                .servers
                .get(&entry.server_name)
                .is_some_and(|server| generation < server.generation)
        {
            return Err(McpError::Protocol(format!(
                "MCP server entry `{}` is no longer installed; refusing stale tool publication",
                entry.server_name
            )));
        }
        for (name, tool) in &imported {
            let existing = self
                .servers
                .iter()
                .filter(|(server, _)| **server != entry.server_name)
                .find_map(|(_, installed)| installed.tools.get(name));
            if let Some(existing) = existing {
                return Err(McpError::Config(format!(
                    "MCP model-facing name collision for `{name}` between tool ids `{}` and `{}`",
                    existing.definition.manifest.id, tool.definition.manifest.id
                )));
            }
        }
        if let Some(installed) = self.servers.get_mut(&entry.server_name) {
            installed.tools = imported;
            installed.generation = generation;
            installed.notifying = notifying;
        }
        self.changed();
        Ok(())
    }

    fn resolve(&self, matches: impl Fn(&str, &ImportedTool) -> bool) -> Option<ResolvedToolTarget> {
        self.servers.values().find_map(|installed| {
            let tool = installed
                .tools
                .iter()
                .find_map(|(name, tool)| matches(name, tool).then_some(tool))?;
            Some(ResolvedToolTarget {
                entry: installed.entry.upgrade()?,
                advertised_name: tool.definition.manifest.tool(),
                native_name: tool.original_name.clone(),
                completion: tool.completion.clone(),
            })
        })
    }
}

#[derive(Clone)]
struct ResolvedToolTarget {
    entry: Arc<McpEntry>,
    advertised_name: String,
    native_name: String,
    completion: RemoteCompletion,
}

impl McpConnectionPool {
    pub fn empty() -> Self {
        Self::empty_with_host_services(McpHostServices::default())
    }

    pub(crate) fn empty_with_host_services(host_services: McpHostServices) -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
            catalog: Arc::new(RwLock::new(InstalledCatalog::default())),
            host_services,
            shut_down: AtomicBool::new(false),
            #[cfg(test)]
            mid_establish_hook: RwLock::new(None),
            #[cfg(test)]
            attach_return_hook: RwLock::new(None),
            #[cfg(test)]
            resolved_target_hook: RwLock::new(None),
            #[cfg(test)]
            advertised_tools_hook: RwLock::new(None),
            #[cfg(test)]
            lifecycle_observer: RwLock::new(None),
        }
    }

    /// Every server is tried eagerly in parallel so tools are available immediately when
    /// servers are up, but a connection failure never aborts construction: the entry stays
    /// registered and follows its configured reconnect mode.
    /// Only configuration errors (a misconfigured server, not an outage) fail the build.
    /// The host must call [`McpConnectionPool::shutdown_all`] to fully reap stdio children;
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
            let entry = McpEntry::new_in_catalog(
                Arc::clone(&pool.catalog),
                name.clone(),
                config,
                pool.host_services.clone(),
            );
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
                    "MCP server unavailable at startup; configured reconnect policy applies"
                );
            }
        }))
        .await;
        Ok(pool)
    }

    /// Like initial pool construction, attach registers the entry before an eager connection
    /// attempt and follows the configured reconnect mode after startup outages.
    /// Only configuration and pool lifecycle errors fail the attach.
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
        let entry = McpEntry::new_in_catalog(
            Arc::clone(&self.catalog),
            server_name.clone(),
            config,
            self.host_services.clone(),
        );
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
        if self.shut_down.load(Ordering::SeqCst) || entry.is_shutting_down() {
            return Err(McpError::PoolShutDown);
        }
        if let Err(err) = connect_result {
            if matches!(err, McpError::PoolShutDown) {
                return Err(err);
            }
            tracing::warn!(
                server = %server_name,
                error = %err,
                "MCP server unavailable during attach; configured reconnect policy applies"
            );
        }
        Ok(())
    }

    pub async fn detach(self: &Arc<Self>, server_name: &str) -> Result<(), McpError> {
        let removed = {
            let mut entries = self.entries.write_recover();
            let removed = entries.remove(server_name);
            if let Some(entry) = &removed {
                self.catalog.write_recover().retire(entry);
            }
            removed
        };
        if let Some(entry) = removed {
            entry.shutdown().await;
        }
        Ok(())
    }

    fn install(
        &self,
        server_name: String,
        entry: Arc<McpEntry>,
    ) -> Result<Option<Arc<McpEntry>>, (Arc<McpEntry>, McpError)> {
        #[cfg(test)]
        if let Some(observer) = self.lifecycle_observer.read_recover().clone() {
            *entry.lifecycle_observer.write_recover() = Some(observer);
        }
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
            let previous = entries.insert(server_name, Arc::clone(&entry));
            self.catalog.write_recover().install(&entry);
            previous
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
        let catalog = self.catalog.read_recover();
        guard
            .values()
            .map(|entry| McpServerStatus {
                server_name: entry.server_name.clone(),
                health: entry.health.read_recover().clone(),
                tool_count: catalog
                    .servers
                    .get(&entry.server_name)
                    .map_or(0, |installed| installed.tools.len()),
            })
            .collect()
    }

    /// Evidence of this server's last failed startup, retained after cleanup.
    /// A successful connection clears it. Content follows the server's opt-in
    /// diagnostics policy; status display strings never include that content.
    pub fn startup_failure(&self, server_name: &str) -> Option<crate::error::McpStartupFailure> {
        self.entries
            .read_recover()
            .get(server_name)
            .and_then(|entry| entry.startup_failure.read_recover().clone())
    }

    /// All advertised tools across every server, each the operation
    /// `<server>.<tool>` in canonical names. Cleanup/truncation collision
    /// groups use `<tool>_h<8-character-id-digest>` for every member. These
    /// are precomputed
    /// `ToolDefinition` clones.
    /// Includes tools of currently disconnected servers (last successful
    /// discovery) so the tool catalog stays stable across an outage.
    pub fn advertised_tools(&self) -> Vec<ToolDefinition> {
        let catalog = self.catalog.read_recover();
        #[cfg(test)]
        let mut hook = self.advertised_tools_hook.read_recover().clone();
        let mut tools = Vec::new();
        for installed in catalog.servers.values() {
            tools.extend(installed.tools.values().map(|tool| tool.definition.clone()));
            #[cfg(test)]
            if let Some(hook) = hook.take() {
                hook();
            }
        }
        tools
    }

    /// Advertised tools belonging to the exact configured server name.
    ///
    /// This preserves the pool's explicit server/tool relation for host views;
    /// callers never need to reconstruct it from normalized display names.
    pub fn advertised_tools_for_server(&self, server_name: &str) -> Vec<ToolDefinition> {
        self.catalog
            .read_recover()
            .servers
            .get(server_name)
            .map(|installed| {
                installed
                    .tools
                    .values()
                    .map(|tool| tool.definition.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        args: &Value,
        context: &AttemptContext<'_>,
    ) -> ToolOutcome {
        if self.shut_down.load(Ordering::SeqCst) {
            return pool_shut_down_failure();
        }
        let Some(target) = self.lookup_by_name(prefixed_name) else {
            return McpCallFailure::UnknownTool {
                name: prefixed_name.to_string(),
            }
            .into();
        };
        self.pause_after_target_resolution().await;
        self.call_resolved_tool(target, args, context).await
    }

    /// Resolve a durable id once, then route the captured raw MCP tool.
    pub async fn call_tool_by_id(
        &self,
        tool_id: &ToolId,
        args: &Value,
        context: &AttemptContext<'_>,
    ) -> ToolOutcome {
        if self.shut_down.load(Ordering::SeqCst) {
            return pool_shut_down_failure();
        }
        let Some(target) = self.lookup_by_id(tool_id) else {
            return McpCallFailure::UnknownToolId {
                tool_id: tool_id.to_string(),
            }
            .into();
        };
        self.pause_after_target_resolution().await;
        self.call_resolved_tool(target, args, context).await
    }

    fn lookup_by_name(&self, prefixed_name: &str) -> Option<ResolvedToolTarget> {
        self.catalog
            .read_recover()
            .resolve(|name, _| name == prefixed_name)
    }

    fn lookup_by_id(&self, tool_id: &ToolId) -> Option<ResolvedToolTarget> {
        self.catalog
            .read_recover()
            .resolve(|_, tool| tool.definition.manifest.id == *tool_id)
    }

    #[cfg(test)]
    fn set_resolved_target_hook(&self, hook: Option<Arc<policy_tests::ActorPauseHook>>) {
        *self.resolved_target_hook.write_recover() = hook;
    }

    #[cfg(test)]
    async fn pause_after_target_resolution(&self) {
        let hook = self.resolved_target_hook.read_recover().clone();
        if let Some(hook) = hook {
            hook.reached.notify_one();
            hook.release.notified().await;
        }
    }

    #[cfg(not(test))]
    async fn pause_after_target_resolution(&self) {}

    /// The events policy of `server_name`, and its live connection when it
    /// has one: what an events request is sent on. `None` for a server the
    /// pool does not hold.
    pub(crate) fn events_connection(
        &self,
        server_name: &str,
    ) -> Option<(
        crate::events::McpServerEvents,
        crate::config::McpDataLimits,
        Option<EventsPeer>,
    )> {
        if self.shut_down.load(Ordering::SeqCst) {
            return None;
        }
        let entry = self.entries.read_recover().get(server_name).cloned()?;
        let service = entry
            .service_snapshot()
            .map(|service| (service.peer.clone(), service.handler.clone()));
        Some((
            entry.config.events.clone(),
            entry.config.data_limits,
            service,
        ))
    }

    /// Tear down all connections in parallel. Call this before dropping the
    /// pool for a graceful shutdown; `Drop` itself cannot await. Every actor is
    /// sent `Shutdown` before the handles are joined concurrently. Each entry's
    /// deadline is its configured graceful period plus post-kill wait and one
    /// second of scheduling margin, so total pool shutdown is approximately the
    /// largest configured bound, not the number of entries multiplied by it.
    ///
    /// A child can be abandoned if it survives the actor's preemptive kill and
    /// bounded reap or if the entry deadline expires mid-reap. The deadline
    /// abort branch reports the live `active_pid`; its PID and reason
    /// are recorded in the actor's health and tracing. No background waitpid sweep is
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
            let entries: Vec<_> = std::mem::take(&mut *guard).into_values().collect();
            self.catalog.write_recover().servers.clear();
            entries
        };
        join_all(entries.iter().map(|entry| entry.shutdown())).await;
    }
}

fn pool_shut_down_failure() -> ToolOutcome {
    McpCallFailure::PoolShutDown.into()
}

fn validate_unique_server_prefixes<'a>(
    server_names: impl IntoIterator<Item = &'a str>,
) -> Result<(), McpError> {
    let mut prefixes = BTreeMap::<String, &'a str>::new();
    for server_name in server_names {
        let prefix = naming::server_segment(server_name);
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
    let incoming_prefix = naming::server_segment(incoming_server);
    existing_server_names
        .into_iter()
        .find(|existing_server| {
            *existing_server != incoming_server
                && naming::server_segment(existing_server) == incoming_prefix
        })
        .map(|existing_server| (existing_server, incoming_prefix))
}

fn prefix_collision_message(existing_server: &str, incoming_server: &str, prefix: &str) -> String {
    format!(
        "MCP servers `{existing_server}` and `{incoming_server}` normalize to the same prefix `{prefix}`"
    )
}

impl McpEntry {
    #[cfg(test)]
    fn new(
        server_name: String,
        config: McpServerConfig,
        host_services: McpHostServices,
    ) -> Arc<Self> {
        let catalog = Arc::new(RwLock::new(InstalledCatalog::default()));
        let entry = Self::new_in_catalog(Arc::clone(&catalog), server_name, config, host_services);
        catalog.write_recover().install(&entry);
        entry
    }

    fn new_in_catalog(
        catalog: Arc<RwLock<InstalledCatalog>>,
        server_name: String,
        config: McpServerConfig,
        host_services: McpHostServices,
    ) -> Arc<Self> {
        let (actor_tx, actor_rx) = tokio::sync::mpsc::unbounded_channel();
        let (published_tx, service) = tokio::sync::watch::channel(None);
        let (refresh_requested, refresh_requests) = tokio::sync::watch::channel(0);
        let active_pid = Arc::new(AtomicU32::new(0));
        Arc::new_cyclic(|weak| {
            let actor = LifecycleActor::new(
                weak.clone(),
                actor_rx,
                refresh_requests,
                published_tx,
                Arc::clone(&active_pid),
                &config,
            );
            let actor_handle = tokio::spawn(actor.run());
            Self {
                catalog,
                server_name,
                config,
                host_services,
                service,
                actor_tx,
                refresh_requested,
                actor_handle: Mutex::new(Some(actor_handle)),
                active_pid,
                health: Arc::new(RwLock::new(McpServerHealth::Connecting)),
                startup_failure: RwLock::new(None),
                reconnect_jitter: RwLock::new(Arc::new(equal_jitter)),
                consecutive_timeouts: AtomicU64::new(0),
                #[cfg(test)]
                mid_establish_hook: RwLock::new(None),
                #[cfg(test)]
                refresh_install_hook: RwLock::new(None),
                #[cfg(test)]
                refresh_notifications: AtomicU64::new(0),
                #[cfg(test)]
                panic_actor_on_quit: AtomicBool::new(false),
                #[cfg(test)]
                shutdown_wedge_pid: AtomicU32::new(0),
                #[cfg(test)]
                never_finish_child_reap: AtomicBool::new(false),
                #[cfg(test)]
                lifecycle_observer: RwLock::new(None),
            }
        })
    }

    fn is_shutting_down(&self) -> bool {
        self.health.read_recover().is_shutting_down()
    }

    fn service_snapshot(&self) -> Option<Arc<PublishedService>> {
        self.service.borrow().clone()
    }

    #[cfg(test)]
    fn replace_imported_tools(
        &self,
        imported: BTreeMap<String, ImportedTool>,
    ) -> Result<(), McpError> {
        self.replace_imported_tools_at(imported, 0, false)
    }
    fn replace_imported_tools_at(
        &self,
        imported: BTreeMap<String, ImportedTool>,
        generation: u64,
        notifying: bool,
    ) -> Result<(), McpError> {
        self.catalog
            .write_recover()
            .replace(self, imported, generation, notifying)
    }

    async fn establish(&self) -> Result<(), McpError> {
        if self.is_shutting_down() {
            return Err(McpError::PoolShutDown);
        }
        let (reply, result) = tokio::sync::oneshot::channel();
        self.actor_tx
            .send(LifecycleCommand::Establish { reply })
            .map_err(|_| McpError::PoolShutDown)?;
        result.await.unwrap_or(Err(McpError::PoolShutDown))
    }

    fn request_tool_refresh(&self, generation: u64) {
        if self.is_shutting_down() {
            return;
        }
        #[cfg(test)]
        self.refresh_notifications.fetch_add(1, Ordering::SeqCst);
        // Every signal marks one latest value dirty. An older service cannot
        // erase a newer generation's notification.
        self.refresh_requested.send_if_modified(|latest| {
            if generation < *latest {
                return false;
            }
            *latest = generation;
            true
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
        observed_generation: u64,
        expired_timeout: Duration,
    ) -> ToolOutcome {
        let server_name = self.server_name.clone();
        let timeout_failure = |deadline| {
            McpCallFailure::CallTimeout {
                server: server_name.clone(),
                timeout_ms: expired_timeout.as_millis() as u64,
                deadline,
            }
            .into()
        };

        // rmcp reports which configured clock expired. Validation requires the
        // wall cap to be strictly greater than the idle duration, so the two
        // expiries are unambiguous. Unknown future timeout sources take the
        // conservative wall-cap path and never affect connection health.
        match expired_timeout {
            timeout if timeout == self.config.call_max_total_timeout() => {
                return timeout_failure(true);
            }
            timeout if timeout == self.config.call_timeout() => {}
            _ => return timeout_failure(true),
        }

        let timeout_failure = || timeout_failure(false);

        match self.config.timeout_disconnect_policy() {
            TimeoutDisconnectPolicy::Never => timeout_failure(),
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
                let Ok(Some(count)) = result.await else {
                    return timeout_failure();
                };
                McpCallFailure::ConnectionLost {
                    server: server_name,
                    cause: McpServiceFailure::ConsecutiveTimeouts { count },
                    after_ms: self.config.reconnect_initial_backoff().as_millis() as u64,
                    shutting_down: self.is_shutting_down(),
                    reconnect_attempts: self.config.reconnect_max_attempts(),
                }
                .into()
            }
        }
    }

    fn record_call_success(&self, observed_generation: u64) {
        let _ = self.actor_tx.send(LifecycleCommand::CallSucceeded {
            generation: observed_generation,
        });
    }

    async fn shutdown(&self) {
        let _ = self.actor_tx.send(LifecycleCommand::Shutdown);
        let handle = self.actor_handle.lock_recover().take();
        let Some(mut handle) = handle else {
            return;
        };
        let shutdown_bound = self.config.shutdown_policy().total_bound();
        let mut abort_on_drop = AbortOnDrop::new(handle.abort_handle());
        match timeout(shutdown_bound, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let reason = format!("MCP lifecycle actor terminated with JoinError: {error}");
                tracing::error!(server = %self.server_name, reason = %reason, "MCP lifecycle actor failed during explicit shutdown");
            }
            Err(_) => {
                let pid = self.active_pid.load(Ordering::SeqCst);
                handle.abort();
                let _ = handle.await;
                let reason = if pid == 0 {
                    format!(
                        "MCP lifecycle actor abandoned: it did not finish within the {shutdown_bound:?} per-entry total shutdown deadline"
                    )
                } else {
                    format!(
                        "MCP stdio child PID {pid} abandoned: lifecycle actor did not finish within the {shutdown_bound:?} per-entry total shutdown deadline"
                    )
                };
                if pid == 0 {
                    tracing::error!(server = %self.server_name, %reason,
                        "MCP explicit shutdown abandoned a wedged lifecycle actor");
                } else {
                    // The child guard reports the abandoned child at ERROR.
                    tracing::debug!(server = %self.server_name, os_process_id = pid, %reason,
                        "MCP lifecycle actor exceeded its shutdown deadline");
                }
            }
        }
        abort_on_drop.disarm();
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_observer(&self) -> Option<crate::service_lifecycle::LifecycleObserver> {
        self.lifecycle_observer.read_recover().clone()
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

/// Import `server_name`'s advertised `tools`, each bounded by `execution`:
/// the server's configured total cap on one call, which its host sets.
fn import_tools(
    server_name: &str,
    tools: Vec<rmcp::model::Tool>,
    execution: Duration,
) -> Result<BTreeMap<String, ImportedTool>, McpError> {
    let raw_names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();
    let names = naming::build_catalog_names(server_name, &raw_names);
    import_tools_with_name_builder(server_name, tools, execution, |_, raw| names[raw].clone())
}

fn import_tools_with_name_builder(
    server_name: &str,
    mut tools: Vec<rmcp::model::Tool>,
    execution: Duration,
    mut build_name: impl FnMut(&str, &str) -> String,
) -> Result<BTreeMap<String, ImportedTool>, McpError> {
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let mut imported = BTreeMap::new();
    let server = naming::server_segment(server_name);
    for tool in tools {
        // MCP tasks are an unsolicited extension result in 2026-07-28,
        // not a required completion mode declared by a tool.
        let completion = RemoteCompletion::Inline;
        let original_name = tool.name.to_string();
        let description = tool
            .description
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        let input_schema = Value::Object((*tool.input_schema).clone());
        let output_schema = mcp_result_schema(tool.output_schema.as_deref());
        // The tool's operation: the server's canonical segment and the
        // tool's canonical name, both NAME-CANON by construction.
        let prefixed = format!("{server}.{}", build_name(server_name, &original_name));
        let tool_id = naming::durable_tool_id(server_name, &original_name);
        let tag = admission::contract_tag(&tool_id, &tool)?;

        let definition = ToolDefinition::raw(
            tool_id,
            prefixed.as_str(),
            description,
            input_schema,
            output_schema,
        )?
        .host_tag(tag)
        .with_execution(execution);
        let imported_tool = ImportedTool {
            original_name,
            definition,
            completion,
        };
        match imported.entry(prefixed.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(imported_tool);
            }
            std::collections::btree_map::Entry::Occupied(existing) => {
                return Err(McpError::Config(format!(
                    "MCP operation collision for `{prefixed}` between tool ids `{}` and `{}`",
                    existing.get().definition.manifest.id,
                    imported_tool.definition.manifest.id
                )));
            }
        }
    }
    Ok(imported)
}

fn mcp_block(kind: &str, fields: impl IntoIterator<Item = (&'static str, ToolValue)>) -> ToolValue {
    let mut block = BTreeMap::from([("type".into(), ToolValue::String(kind.into()))]);
    block.extend(fields.into_iter().map(|(key, value)| (key.into(), value)));
    ToolValue::Object(block)
}

async fn tool_result_from_rmcp(
    result: rmcp::model::CallToolResult,
    context: &AttemptContext<'_>,
) -> ToolOutcome {
    let is_error = result.is_error.unwrap_or(false);
    let structured = result.structured_content;
    let structured_projection = structured.clone();
    let mut text_parts = Vec::new();
    let mut content_items = Vec::new();
    let mut view_blocks = Vec::new();
    let mut view_only_json_copy = true;

    for content in result.content {
        let annotations = match &content {
            ContentBlock::Text(content) => content.annotations.as_ref(),
            ContentBlock::Image(content) => content.annotations.as_ref(),
            ContentBlock::Audio(content) => content.annotations.as_ref(),
            ContentBlock::Resource(content) => content.annotations.as_ref(),
            ContentBlock::ResourceLink(content) => content.annotations.as_ref(),
            _ => None,
        };
        if annotations
            .and_then(|annotations| annotations.audience.as_ref())
            .is_some_and(|audience| !audience.contains(&Role::Assistant))
        {
            continue;
        }
        let priority = match annotations
            .and_then(|annotations| annotations.priority)
            .map(|value| lash_core::ToolViewPriority::new(f64::from(value)))
            .transpose()
        {
            Ok(priority) => priority,
            Err(error) => {
                return ToolOutcome::failure(lash_core::ToolFailure::new(
                    "mcp_nonfinite_view_priority",
                    error.to_string(),
                ));
            }
        };
        let meta = ToolViewMeta {
            priority,
            last_modified: annotations
                .and_then(|annotations| annotations.last_modified.as_ref())
                .map(|timestamp| {
                    chrono::DateTime::parse_from_rfc3339(timestamp)
                        .map(|timestamp| timestamp.with_timezone(&chrono::Utc).to_rfc3339())
                        .unwrap_or_else(|_| timestamp.clone())
                }),
        };
        let raw = content;
        let json_copy = matches!(&raw, ContentBlock::Text(text) if structured.as_ref().is_some_and(|value| {
            serde_json::from_str::<Value>(&text.text).ok().as_ref() == Some(value)
        }));
        view_only_json_copy &= json_copy;
        match raw {
            ContentBlock::Text(text) => {
                text_parts.push(text.text.clone());
                view_blocks.push(ToolViewBlock::Text {
                    text: text.text.clone(),
                    meta,
                });
                if !json_copy {
                    content_items.push(mcp_block("text", [("text", ToolValue::String(text.text))]));
                }
            }
            ContentBlock::Image(image) => {
                let reference =
                    match store_mcp_attachment(context, &image.data, &image.mime_type, "MCP image")
                        .await
                    {
                        Ok(reference) => reference,
                        Err(result) => return result,
                    };
                let source = reference;
                view_blocks.push(ToolViewBlock::Attachment {
                    reference: source.clone(),
                    meta,
                });
                content_items.push(mcp_block(
                    "image",
                    [
                        ("attachment", ToolValue::Attachment(source)),
                        ("mimeType", ToolValue::String(image.mime_type)),
                    ],
                ));
            }
            ContentBlock::Audio(audio) => {
                let reference =
                    match store_mcp_attachment(context, &audio.data, &audio.mime_type, "MCP audio")
                        .await
                    {
                        Ok(reference) => reference,
                        Err(result) => return result,
                    };
                let source = reference;
                view_blocks.push(ToolViewBlock::Attachment {
                    reference: source.clone(),
                    meta,
                });
                content_items.push(mcp_block(
                    "audio",
                    [
                        ("attachment", ToolValue::Attachment(source)),
                        ("mimeType", ToolValue::String(audio.mime_type)),
                    ],
                ));
            }
            ContentBlock::Resource(resource) => match resource.resource {
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".into());
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
                    let source = reference;
                    view_blocks.push(ToolViewBlock::Attachment {
                        reference: source.clone(),
                        meta,
                    });
                    content_items.push(mcp_block(
                        "resource",
                        [
                            ("uri", ToolValue::String(uri)),
                            ("mimeType", ToolValue::String(mime_type)),
                            ("attachment", ToolValue::Attachment(source)),
                        ],
                    ));
                }
                ResourceContents::TextResourceContents {
                    uri,
                    mime_type,
                    text,
                    ..
                } => {
                    view_blocks.push(ToolViewBlock::Text {
                        text: format!("{uri}\n{text}"),
                        meta,
                    });
                    let mut fields = vec![
                        ("uri", ToolValue::String(uri)),
                        ("text", ToolValue::String(text)),
                    ];
                    if let Some(mime_type) = mime_type {
                        fields.push(("mimeType", ToolValue::String(mime_type)));
                    }
                    content_items.push(mcp_block("resource", fields));
                }
                _ => return McpCallFailure::UnexpectedResponse.into(),
            },
            ContentBlock::ResourceLink(link) => {
                view_blocks.push(ToolViewBlock::ResourceLink {
                    uri: link.uri.clone(),
                    name: link.name.clone(),
                    title: link.title.clone(),
                    description: link.description.clone(),
                    mime_type: link.mime_type.clone(),
                    meta,
                });
                let mut fields = vec![
                    ("uri", ToolValue::String(link.uri)),
                    ("name", ToolValue::String(link.name)),
                ];
                if let Some(title) = link.title {
                    fields.push(("title", ToolValue::String(title)));
                }
                if let Some(description) = link.description {
                    fields.push(("description", ToolValue::String(description)));
                }
                if let Some(mime_type) = link.mime_type {
                    fields.push(("mimeType", ToolValue::String(mime_type)));
                }
                content_items.push(mcp_block("resource_link", fields));
            }
            _ => return McpCallFailure::UnexpectedResponse.into(),
        }
    }

    let mut fields = BTreeMap::from([("content".into(), ToolValue::Array(content_items))]);
    if let Some(structured) = structured {
        fields.insert(
            "structuredContent".into(),
            ToolValue::untrusted_json(structured),
        );
    }
    let value = ToolValue::Object(fields);
    if is_error {
        McpCallFailure::ToolError {
            message: if text_parts.is_empty() {
                "MCP tool returned an error".into()
            } else {
                text_parts.join("\n\n")
            },
            content: value,
        }
        .into()
    } else {
        let output = ToolCallOutput::success_tool_value(value);
        ToolOutcome::from_output(
            if let Some(structured) =
                structured_projection.filter(|_| view_blocks.is_empty() || view_only_json_copy)
            {
                output.with_projection_value(structured)
            } else {
                output.with_view(ToolView {
                    blocks: view_blocks,
                })
            },
        )
    }
}

async fn store_mcp_attachment(
    context: &AttemptContext<'_>,
    encoded: &str,
    mime_type: &str,
    label: &str,
) -> Result<lash_core::AttachmentRef, ToolOutcome> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| ToolOutcome::from(McpCallFailure::AttachmentDecode { cause: err.into() }))?;
    let media_type = MediaType::parse(mime_type).map_err(|_err| {
        ToolOutcome::from(McpCallFailure::AttachmentMime {
            media_type: mime_type.to_string(),
        })
    })?;
    context
        .attachments()
        .put(
            data,
            AttachmentCreateMeta::new(media_type, None, Some(label.to_string())),
        )
        .await
        .map(|receipt| receipt.reference)
        .map_err(|err| {
            ToolOutcome::from(McpCallFailure::AttachmentStore {
                cause: err.retention_failure(),
                diagnostic: err.to_string(),
            })
        })
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
        if self.is_shutting_down() {
            return;
        }
        if let Some(handle) = self.actor_handle.get_mut().recover().take() {
            handle.abort();
        }
        // The actor-owned StdioChildGuard kills and reports its own child.
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod policy_tests;

#[cfg(test)]
#[path = "pool_unit_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "catalog_peer_tests.rs"]
mod catalog_peer_tests;
