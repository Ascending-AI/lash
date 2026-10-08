//! Single-owner lifecycle actor for one MCP pool entry.
//!
//! This task is the only owner of an entry's `RunningService`, stdio child,
//! reconnect pacing, and generation allocator. Callers observe a cheap
//! published peer snapshot and submit lifecycle observations as messages.

use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use lash_sansio::sync::{MutexExt, RwLockExt};
use rmcp::service::QuitReason;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};

use super::admission;
use super::{
    McpEntry, McpServerFault, McpServerHealth, McpToolListRefresh, PublishedService, import_tools,
};
use crate::config::McpShutdownPolicy;
use crate::error::McpError;
use crate::service_lifecycle::{ConnectingService, StdioChildGuard, connect_service};
use crate::stdio_transport::StdioCloseCause;

pub(super) enum LifecycleCommand {
    Establish {
        reply: oneshot::Sender<Result<(), McpError>>,
    },
    Disconnect {
        generation: u64,
        cause: String,
    },
    CallSucceeded {
        generation: u64,
    },
    CallTimedOut {
        generation: u64,
        reply: oneshot::Sender<Option<u64>>,
    },
    Shutdown,
}

pub(super) struct LifecycleActor {
    entry: Weak<McpEntry>,
    commands: mpsc::UnboundedReceiver<LifecycleCommand>,
    refresh_requests: watch::Receiver<u64>,
    published: watch::Sender<Option<Arc<PublishedService>>>,
    active_pid: Arc<AtomicU32>,
    shutdown_policy: McpShutdownPolicy,
    generation: u64,
    reconnect_backoff: Duration,
    reconnect_attempts: u64,
    reconnect_at: Option<Instant>,
    keepalive_at: Option<Instant>,
    finished: bool,
}

enum ConnectionExit {
    Failed,
    Disconnected,
    Shutdown,
}

#[derive(Clone, Copy)]
enum CommandPhase {
    Idle,
    Handshake,
    Discovery,
    Connected {
        generation: u64,
    },
    Probe {
        generation: u64,
    },
    Reaping,
    #[cfg(test)]
    TestPause,
}

impl CommandPhase {
    fn observes(self, generation: u64) -> bool {
        matches!(
            self,
            Self::Connected {
                generation: current
            } | Self::Probe {
                generation: current
            } if current == generation
        )
    }
}

enum CommandAction {
    Establish {
        reply: oneshot::Sender<Result<(), McpError>>,
    },
    Disconnect {
        cause: String,
    },
    CallSucceeded,
    CallTimedOut {
        reply: oneshot::Sender<Option<u64>>,
    },
    Shutdown,
    Continue,
}

type WaitingFuture =
    Pin<Box<dyn Future<Output = Result<QuitReason, tokio::task::JoinError>> + Send + 'static>>;

enum ServiceWaiting {
    Pending(WaitingFuture),
    Complete,
}

impl ServiceWaiting {
    fn new(waiting: WaitingFuture) -> Self {
        Self::Pending(waiting)
    }

    async fn wait_for_cleanup(self, graceful_period: Duration) {
        if let Self::Pending(mut waiting) = self {
            let _ = timeout(graceful_period, waiting.as_mut()).await;
        }
    }
}

impl Future for ServiceWaiting {
    type Output = Result<QuitReason, tokio::task::JoinError>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let waiting = self.get_mut();
        let Self::Pending(future) = waiting else {
            panic!("service waiting future polled after completion");
        };
        match future.as_mut().poll(cx) {
            std::task::Poll::Ready(reason) => {
                *waiting = Self::Complete;
                std::task::Poll::Ready(reason)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

type CatalogRefreshResult =
    Result<Result<Vec<rmcp::model::Tool>, McpError>, tokio::time::error::Elapsed>;
type CatalogRefresh = Pin<Box<dyn Future<Output = CatalogRefreshResult> + Send>>;

async fn poll_refresh(refresh: &mut Option<CatalogRefresh>) -> CatalogRefreshResult {
    match refresh {
        Some(future) => future.as_mut().await,
        None => pending().await,
    }
}

struct Connection {
    cancellation: rmcp::service::RunningServiceCancellationToken,
    request_tasks: Arc<crate::host::McpHostRequestTasks>,
    waiting: ServiceWaiting,
    child: Option<StdioChildGuard>,
    stdio_close_cause: Option<StdioCloseCause>,
    refresh: Option<CatalogRefresh>,
}

/// The transport's recorded read cause beats rmcp's generic quit reason: an
/// over-limit inbound message ends `receive` as `None`, which `waiting`
/// reports only as an unnamed transport close.
fn quit_cause(
    connection: &Connection,
    server_name: &str,
    context: &str,
    reason: &Result<QuitReason, tokio::task::JoinError>,
) -> String {
    let recorded = connection
        .stdio_close_cause
        .as_ref()
        .and_then(|cell| cell.lock_recover().as_ref().map(ToString::to_string));
    match recorded {
        Some(cause) => format!("MCP server `{server_name}` service quit{context}: {cause}"),
        None => format!("MCP server `{server_name}` service quit{context}: {reason:?}"),
    }
}

impl Connection {
    // Cooperative terminal paths consume this owner and await cleanup. If the
    // actor itself is aborted, dropping the child guard retains the pool's
    // documented forced-abandonment kill-and-log fallback.
    async fn cancel_and_reap(mut self, actor: &mut LifecycleActor, server_name: &str) -> bool {
        // Dropping the actor-owned future cancels discovery and any paused
        // publication before transport or process cleanup begins.
        self.refresh = None;
        if let Some(child) = self.child.as_mut() {
            child.begin_bounded_cleanup();
        }
        self.cancellation.cancel();
        let entry = actor.entry.clone();
        let active_pid = Arc::clone(&actor.active_pid);
        let shutdown_policy = actor.shutdown_policy;
        let cleanup = async move {
            if self.child.is_some() {
                // Host request cancellation cannot delay process cleanup: the
                // two shutdown responsibilities advance concurrently.
                let (_, ()) = tokio::join!(
                    self.request_tasks.shutdown(),
                    reap_child(entry, active_pid, self.child, shutdown_policy,),
                );
            } else {
                // HTTP has no child to reap, but still gets the configured
                // grace for its transport task to drain. A waiting future that
                // already completed returns immediately without being re-polled.
                shutdown_http_connection(
                    self.request_tasks,
                    self.waiting,
                    shutdown_policy.graceful_period,
                )
                .await;
            }
        };
        tokio::pin!(cleanup);

        let mut shutdown_observed = false;
        loop {
            tokio::select! {
                () = &mut cleanup => return shutdown_observed,
                command = actor.commands.recv(), if !shutdown_observed => {
                    match LifecycleActor::reduce_command(
                        CommandPhase::Reaping,
                        command,
                        server_name,
                    ) {
                        CommandAction::Shutdown => {
                            actor.begin_shutdown();
                            // Keep the already-running reap future alive. The
                            // entry deadline includes both policy durations plus margin.
                            shutdown_observed = true;
                        }
                        CommandAction::Continue => {}
                        _ => unreachable!("reaping command reducer returned an active action"),
                    }
                }
            }
        }
    }
}

impl LifecycleActor {
    fn catalog_refresh(
        &self,
        peer: rmcp::service::Peer<rmcp::service::RoleClient>,
        deadline: Duration,
    ) -> CatalogRefresh {
        #[cfg(test)]
        let entry = self.entry.clone();
        Box::pin(async move {
            let result = timeout(deadline, super::catalog::discover_tools(&peer)).await;
            #[cfg(test)]
            if matches!(result, Ok(Ok(_)))
                && let Some(entry) = entry.upgrade()
            {
                entry.pause_before_refresh_install().await;
            }
            result
        })
    }

    fn reduce_command(
        phase: CommandPhase,
        command: Option<LifecycleCommand>,
        server_name: &str,
    ) -> CommandAction {
        let Some(command) = command else {
            return CommandAction::Shutdown;
        };
        match command {
            LifecycleCommand::Establish { reply } => match phase {
                CommandPhase::Idle => CommandAction::Establish { reply },
                CommandPhase::Connected { .. } | CommandPhase::Probe { .. } => {
                    let _ = reply.send(Ok(()));
                    CommandAction::Continue
                }
                CommandPhase::Handshake | CommandPhase::Discovery => {
                    let _ = reply.send(Err(McpError::Protocol(format!(
                        "MCP connection for `{server_name}` is already being established"
                    ))));
                    CommandAction::Continue
                }
                CommandPhase::Reaping => {
                    let _ = reply.send(Err(McpError::Protocol(
                        "MCP connection is restarting or being reaped".to_string(),
                    )));
                    CommandAction::Continue
                }
                #[cfg(test)]
                CommandPhase::TestPause => {
                    let _ = reply.send(Err(McpError::Protocol(
                        "MCP connection is already being established".to_string(),
                    )));
                    CommandAction::Continue
                }
            },
            LifecycleCommand::Disconnect { generation, cause } => {
                if phase.observes(generation) {
                    CommandAction::Disconnect { cause }
                } else {
                    CommandAction::Continue
                }
            }
            LifecycleCommand::CallSucceeded { generation } => {
                if phase.observes(generation) {
                    CommandAction::CallSucceeded
                } else {
                    CommandAction::Continue
                }
            }
            LifecycleCommand::CallTimedOut { generation, reply } => {
                if phase.observes(generation) {
                    CommandAction::CallTimedOut { reply }
                } else {
                    let _ = reply.send(None);
                    CommandAction::Continue
                }
            }
            LifecycleCommand::Shutdown => CommandAction::Shutdown,
        }
    }

    pub(super) fn new(
        entry: Weak<McpEntry>,
        commands: mpsc::UnboundedReceiver<LifecycleCommand>,
        refresh_requests: watch::Receiver<u64>,
        published: watch::Sender<Option<Arc<PublishedService>>>,
        active_pid: Arc<AtomicU32>,
        config: &crate::config::McpServerConfig,
    ) -> Self {
        let keepalive_interval = config.liveness_probe_interval();
        Self {
            entry,
            commands,
            refresh_requests,
            published,
            active_pid,
            shutdown_policy: *config.shutdown_policy(),
            finished: false,
            generation: 0,
            reconnect_backoff: config.reconnect_initial_backoff(),
            reconnect_attempts: 0,
            reconnect_at: None,
            keepalive_at: (!keepalive_interval.is_zero())
                .then(|| Instant::now() + keepalive_interval),
        }
    }

    pub(super) async fn run(mut self) {
        self.run_inner().await;
        self.finished = true;
    }

    async fn run_inner(&mut self) {
        loop {
            let reconnect_at = self.reconnect_at;
            let keepalive_at = self.keepalive_at;
            tokio::select! {
                command = self.commands.recv() => {
                    match Self::reduce_command(CommandPhase::Idle, command, "") {
                        CommandAction::Establish { reply } => {
                            self.reconnect_at = None;
                            self.reconnect_attempts = 0;
                            self.set_health(McpServerHealth::Connecting);
                            if matches!(self.connect_and_run(Some(reply)).await, ConnectionExit::Shutdown) {
                                return;
                            }
                            self.schedule_reconnect();
                        }
                        CommandAction::Shutdown => {
                            self.begin_shutdown();
                            self.wedge_shutdown_if_injected().await;
                            return;
                        }
                        CommandAction::Continue => {}
                        _ => unreachable!("idle command reducer returned an active connection action"),
                    }
                }
                () = sleep_until(reconnect_at), if reconnect_at.is_some() => {
                    self.reconnect_at = None;
                    self.reconnect_attempts = self.reconnect_attempts.saturating_add(1);
                    self.advance_reconnect_backoff();
                    match self.connect_and_run(None).await {
                        ConnectionExit::Shutdown => return,
                        ConnectionExit::Disconnected => self.schedule_reconnect(),
                        ConnectionExit::Failed => {
                            let attempts = self.entry.upgrade()
                                .map_or(crate::ReconnectAttempts::Disabled, |entry| entry.config.reconnect_max_attempts());
                            if !attempts.allows(self.reconnect_attempts) {
                                self.record_exhaustion();
                            } else {
                                self.schedule_reconnect();
                            }
                        }
                    }
                }
                () = sleep_until(keepalive_at), if keepalive_at.is_some() => {
                    self.advance_keepalive_deadline();
                    self.rearm_exhausted_reconnect();
                }
            }
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "a u64 generation increments once per connection; exhaustion is unreachable within any real process lifetime"
    )]
    async fn connect_and_run(
        &mut self,
        initial_reply: Option<oneshot::Sender<Result<(), McpError>>>,
    ) -> ConnectionExit {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("MCP service generation exhausted");
        let generation = self.generation;
        let Some(entry) = self.entry.upgrade() else {
            return ConnectionExit::Shutdown;
        };
        let server_name = entry.server_name.clone();
        let config = entry.config.clone();
        let host_services = entry.host_services.clone();
        let shutdown_requested = Arc::clone(&entry.health);
        let startup_timeout = config.startup_timeout();
        let refresh = Arc::new(McpToolListRefresh {
            entry: Arc::downgrade(&entry),
            service_generation: generation,
        });
        #[cfg(test)]
        let never_finish_child_reap = entry.never_finish_child_reap.load(Ordering::SeqCst);
        #[cfg(test)]
        let lifecycle_observer = entry.lifecycle_observer();
        drop(entry);

        let ConnectingService {
            handshake,
            mut stdio_child,
            stdio_close_cause,
        } = match connect_service(
            &server_name,
            &config,
            host_services,
            refresh,
            Arc::clone(&self.active_pid),
            shutdown_requested,
        ) {
            Ok(connecting) => connecting,
            Err(error) => {
                self.active_pid.store(0, Ordering::SeqCst);
                self.record_mcp_error(&error);
                send_result(initial_reply, Err(error));
                return ConnectionExit::Failed;
            }
        };
        #[cfg(test)]
        if never_finish_child_reap && let Some(child) = stdio_child.as_mut() {
            child.never_finish_reap();
        }
        #[cfg(test)]
        if let Some(observer) = lifecycle_observer
            && let Some(child) = stdio_child.as_mut()
        {
            let _ = observer
                .send(crate::service_lifecycle::LifecycleEvent::Spawned { pid: child.pid() });
            child.observe(observer);
        }
        let mut connection_attempt = Box::pin(timeout(startup_timeout, handshake));
        let connected = loop {
            tokio::select! {
                result = &mut connection_attempt => break result,
                command = self.commands.recv() => match Self::reduce_command(
                    CommandPhase::Handshake,
                    command,
                    &server_name,
                ) {
                    CommandAction::Shutdown => {
                        self.begin_shutdown();
                        drop(connection_attempt);
                        if let Some(pid) = stdio_child.as_ref().map(StdioChildGuard::pid) {
                            self.record_error(format!(
                                "MCP stdio child PID {pid} handshake interrupted by pool shutdown"
                            ));
                        }
                        self.reap_child(stdio_child.take()).await;
                        send_shutdown(initial_reply);
                        return ConnectionExit::Shutdown;
                    }
                    CommandAction::Continue => {}
                    _ => unreachable!("handshake command reducer returned an active action"),
                }
            }
        };
        let running = match connected {
            Ok(Ok(running)) => running,
            Ok(Err(error)) => {
                self.record_mcp_error(&error);
                self.reap_child(stdio_child.take()).await;
                send_result(initial_reply, Err(error));
                return ConnectionExit::Failed;
            }
            Err(_) => {
                drop(connection_attempt);
                let error = McpError::StartupTimeout {
                    server: server_name.clone(),
                    timeout_ms: startup_timeout.as_millis() as u64,
                };
                self.record_mcp_error(&error);
                self.reap_child(stdio_child.take()).await;
                send_result(initial_reply, Err(error));
                return ConnectionExit::Failed;
            }
        };

        let peer = running.peer().clone();
        let mut connection = Connection {
            cancellation: running.cancellation_token(),
            request_tasks: running.service().request_tasks(),
            waiting: ServiceWaiting::new(Box::pin(running.waiting())),
            child: stdio_child.take(),
            stdio_close_cause,
            refresh: None,
        };
        if self.pause_mid_establish().await {
            connection.cancel_and_reap(self, &server_name).await;
            send_shutdown(initial_reply);
            return ConnectionExit::Shutdown;
        }

        let discovery = timeout(startup_timeout, super::catalog::discover_tools(&peer));
        tokio::pin!(discovery);
        let tools = loop {
            tokio::select! {
                biased;
                result = &mut discovery => {
                    break match result {
                        Ok(Ok(tools)) => tools,
                        Ok(Err(error)) => {
                            let error = McpError::Protocol(format!("list_tools failed: {error}"));
                            self.record_mcp_error(&error);
                            let shutdown = connection.cancel_and_reap(self, &server_name).await;
                            if shutdown {
                                send_shutdown(initial_reply);
                                return ConnectionExit::Shutdown;
                            }
                            send_result(initial_reply, Err(error));
                            return ConnectionExit::Failed;
                        }
                        Err(_) => {
                            let error = McpError::StartupTimeout {
                                server: server_name.clone(),
                                timeout_ms: startup_timeout.as_millis() as u64,
                            };
                            self.record_mcp_error(&error);
                            let shutdown = connection.cancel_and_reap(self, &server_name).await;
                            if shutdown {
                                send_shutdown(initial_reply);
                                return ConnectionExit::Shutdown;
                            }
                            send_result(initial_reply, Err(error));
                            return ConnectionExit::Failed;
                        }
                    }
                }
                reason = &mut connection.waiting => {
                    let cause = quit_cause(&connection, &server_name, " during discovery", &reason);
                    self.record_error(cause.clone());
                    let shutdown = connection.cancel_and_reap(self, &server_name).await;
                    if shutdown {
                        send_shutdown(initial_reply);
                        return ConnectionExit::Shutdown;
                    }
                    send_result(initial_reply, Err(McpError::Protocol(cause)));
                    return ConnectionExit::Failed;
                }
                command = self.commands.recv() => match Self::reduce_command(
                    CommandPhase::Discovery,
                    command,
                    &server_name,
                ) {
                    CommandAction::Shutdown => {
                        self.begin_shutdown();
                        connection.cancel_and_reap(self, &server_name).await;
                        send_shutdown(initial_reply);
                        return ConnectionExit::Shutdown;
                    }
                    CommandAction::Continue => {}
                    _ => unreachable!("discovery command reducer returned an active action"),
                }
            }
        };

        let imported = match self
            .entry
            .upgrade()
            .ok_or(McpError::PoolShutDown)
            .and_then(|entry| {
                let tools =
                    import_tools(&server_name, tools, entry.config.call_max_total_timeout())?;
                admission::bind_imported_tools(tools, &entry, &peer)
            }) {
            Ok(imported) => imported,
            Err(error) => {
                self.record_mcp_error(&error);
                let shutdown = connection.cancel_and_reap(self, &server_name).await;
                if shutdown {
                    send_shutdown(initial_reply);
                    return ConnectionExit::Shutdown;
                }
                send_result(initial_reply, Err(error));
                return ConnectionExit::Failed;
            }
        };
        let Some(entry) = self.entry.upgrade() else {
            let _ = connection.cancel_and_reap(self, &server_name).await;
            return ConnectionExit::Shutdown;
        };
        if let Err(error) = entry.replace_imported_tools(imported) {
            drop(entry);
            self.record_mcp_error(&error);
            let shutdown = connection.cancel_and_reap(self, &server_name).await;
            if shutdown {
                send_shutdown(initial_reply);
                return ConnectionExit::Shutdown;
            }
            send_result(initial_reply, Err(error));
            return ConnectionExit::Failed;
        }
        entry.consecutive_timeouts.store(0, Ordering::SeqCst);
        self.set_health(McpServerHealth::Connected {
            catalog_error: None,
        });
        self.reconnect_attempts = 0;
        drop(entry);
        self.published.send_replace(Some(Arc::new(PublishedService {
            peer: peer.clone(),
            generation,
        })));
        send_result(initial_reply, Ok(()));

        let healthy_dwell = self
            .entry
            .upgrade()
            .map_or(Duration::ZERO, |entry| entry.config.reconnect_max_backoff());
        let healthy = tokio::time::sleep(healthy_dwell);
        tokio::pin!(healthy);
        let mut healthy_observed = false;
        loop {
            let keepalive_at = self.keepalive_at;
            tokio::select! {
                result = poll_refresh(&mut connection.refresh), if connection.refresh.is_some() => {
                    connection.refresh = None;
                    let Ok(result) = result else {
                        // rmcp retains a responder for an unanswered request.
                        // End this service on timeout so repeated storms cannot
                        // accumulate those responders across refresh attempts.
                        self.unpublish(generation);
                        self.record_error(format!("MCP catalog refresh timed out for `{server_name}`"));
                        let shutdown = connection.cancel_and_reap(self, &server_name).await;
                        return if shutdown { ConnectionExit::Shutdown } else { ConnectionExit::Disconnected };
                    };
                    match result
                        .and_then(|tools| {
                            let entry = self.entry.upgrade().ok_or(McpError::PoolShutDown)?;
                            let tools = import_tools(
                                &server_name,
                                tools,
                                entry.config.call_max_total_timeout(),
                            )?;
                            admission::bind_imported_tools(tools, &entry, &peer)
                        })
                        .and_then(|tools| self.entry.upgrade()
                            .ok_or(McpError::PoolShutDown)?
                            .replace_imported_tools(tools))
                    {
                        Ok(()) => {}
                        Err(error) => {
                            tracing::warn!(server = %server_name, error = %error, "MCP tools/list refresh refused");
                            self.record_mcp_error(&error);
                        }
                    }
                }
                changed = self.refresh_requests.changed(), if connection.refresh.is_none() => {
                    if changed.is_err() {
                        self.unpublish(generation);
                        let _ = connection.cancel_and_reap(self, &server_name).await;
                        return ConnectionExit::Shutdown;
                    }
                    if *self.refresh_requests.borrow_and_update() == generation {
                        connection.refresh = Some(self.catalog_refresh(peer.clone(), startup_timeout));
                    }
                }
                reason = &mut connection.waiting => {
                    let cause = quit_cause(&connection, &server_name, "", &reason);
                    self.record_error(cause);
                    self.unpublish(generation);
                    let shutdown = connection.cancel_and_reap(self, &server_name).await;
                    self.maybe_panic_on_service_quit();
                    return if shutdown {
                        ConnectionExit::Shutdown
                    } else {
                        ConnectionExit::Disconnected
                    };
                }
                command = self.commands.recv() => match Self::reduce_command(
                    CommandPhase::Connected { generation },
                    command,
                    &server_name,
                ) {
                    CommandAction::Disconnect { cause } => {
                        self.unpublish(generation);
                        self.record_error(cause);
                        let shutdown = connection.cancel_and_reap(self, &server_name).await;
                        return if shutdown {
                            ConnectionExit::Shutdown
                        } else {
                            ConnectionExit::Disconnected
                        };
                    }
                    CommandAction::Shutdown => {
                        self.begin_shutdown();
                        self.unpublish(generation);
                        let _ = connection.cancel_and_reap(self, &server_name).await;
                        return ConnectionExit::Shutdown;
                    }
                    CommandAction::CallSucceeded => {
                        if let Some(entry) = self.entry.upgrade() {
                            entry.consecutive_timeouts.store(0, Ordering::SeqCst);
                            self.set_health(McpServerHealth::Connected { catalog_error: None });
                        }
                    }
                    CommandAction::CallTimedOut { reply } => {
                        let Some(entry) = self.entry.upgrade() else {
                            let _ = reply.send(None);
                            self.unpublish(generation);
                            let _ = connection.cancel_and_reap(self, &server_name).await;
                            return ConnectionExit::Shutdown;
                        };
                        let consecutive = entry.consecutive_timeouts.fetch_add(1, Ordering::SeqCst) + 1;
                        let threshold = entry.config.consecutive_timeouts_before_disconnect();
                        if consecutive < threshold {
                            let _ = reply.send(None);
                            continue;
                        }
                        let cause = format!(
                            "MCP server `{server_name}` reached {consecutive} consecutive call timeouts"
                        );
                        let _ = reply.send(Some(consecutive));
                        drop(entry);
                        self.unpublish(generation);
                        self.record_error(cause);
                        let shutdown = connection.cancel_and_reap(self, &server_name).await;
                        return if shutdown {
                            ConnectionExit::Shutdown
                        } else {
                            ConnectionExit::Disconnected
                        };
                    }
                    CommandAction::Continue => {}
                    CommandAction::Establish { .. } => {
                        unreachable!("connected reducer returned an establish action")
                    }
                },
                () = &mut healthy, if !healthy_observed => {
                    healthy_observed = true;
                    if self.current_generation() == Some(generation) {
                        self.reconnect_backoff = self.entry.upgrade().map_or(
                            self.reconnect_backoff,
                            |entry| entry.config.reconnect_initial_backoff(),
                        );
                    }
                }
                () = sleep_until(keepalive_at), if keepalive_at.is_some() => {
                    self.advance_keepalive_deadline();
                    let Some(entry) = self.entry.upgrade() else {
                        self.unpublish(generation);
                        let _ = connection.cancel_and_reap(self, &server_name).await;
                        return ConnectionExit::Shutdown;
                    };
                    if !entry.peer_supports_ping(&peer) {
                        self.keepalive_at = None;
                        continue;
                    }
                    let failure = if peer.is_transport_closed() {
                        Some("transport closed before liveness probe".to_string())
                    } else {
                        let mut probe = Box::pin(entry.probe_peer(&peer));
                        loop {
                            #[cfg(test)]
                            pause_probe_select_if_injected(&entry).await;
                            tokio::select! {
                                biased;
                                () = &mut healthy, if !healthy_observed => {
                                    healthy_observed = true;
                                    if self.current_generation() == Some(generation) {
                                        self.reconnect_backoff = self.entry.upgrade().map_or(
                                            self.reconnect_backoff,
                                            |entry| entry.config.reconnect_initial_backoff(),
                                        );
                                    }
                                }
                                reason = &mut connection.waiting => {
                                    let cause = format!("MCP server `{server_name}` service quit: {reason:?}");
                                    drop(probe);
                                    drop(entry);
                                    self.record_error(cause);
                                    self.unpublish(generation);
                                    let shutdown = connection.cancel_and_reap(self, &server_name).await;
                                    self.maybe_panic_on_service_quit();
                                    return if shutdown {
                                        ConnectionExit::Shutdown
                                    } else {
                                        ConnectionExit::Disconnected
                                    };
                                }
                                result = &mut probe => break result.err().map(|error| error.to_string()),
                                command = self.commands.recv() => match Self::reduce_command(
                                    CommandPhase::Probe { generation },
                                    command,
                                    &server_name,
                                ) {
                                    CommandAction::Shutdown => {
                                        self.begin_shutdown();
                                        drop(probe);
                                        drop(entry);
                                        self.unpublish(generation);
                                        let _ = connection.cancel_and_reap(self, &server_name).await;
                                        return ConnectionExit::Shutdown;
                                    }
                                    CommandAction::Disconnect { cause } => {
                                        drop(probe);
                                        drop(entry);
                                        self.unpublish(generation);
                                        self.record_error(cause);
                                        let shutdown = connection.cancel_and_reap(self, &server_name).await;
                                        return if shutdown {
                                            ConnectionExit::Shutdown
                                        } else {
                                            ConnectionExit::Disconnected
                                        };
                                    }
                                    CommandAction::CallSucceeded => {
                                        entry.consecutive_timeouts.store(0, Ordering::SeqCst);
                                        self.set_health(McpServerHealth::Connected { catalog_error: None });
                                    }
                                    CommandAction::CallTimedOut { reply } => {
                                        let consecutive = entry.consecutive_timeouts.fetch_add(1, Ordering::SeqCst) + 1;
                                        let threshold = entry.config.consecutive_timeouts_before_disconnect();
                                        if consecutive < threshold {
                                            let _ = reply.send(None);
                                            continue;
                                        }
                                        let cause = format!(
                                            "MCP server `{server_name}` reached {consecutive} consecutive call timeouts"
                                        );
                                        let _ = reply.send(Some(consecutive));
                                        drop(probe);
                                        drop(entry);
                                        self.unpublish(generation);
                                        self.record_error(cause);
                                        let shutdown = connection.cancel_and_reap(self, &server_name).await;
                                        return if shutdown {
                                            ConnectionExit::Shutdown
                                        } else {
                                            ConnectionExit::Disconnected
                                        };
                                    }
                                    CommandAction::Continue => {}
                                    CommandAction::Establish { .. } => {
                                        unreachable!("probe reducer returned an establish action")
                                    }
                                }
                            }
                        }
                    };
                    #[cfg(test)]
                    entry.probe_completed.notify_one();
                    drop(entry);
                    if let Some(failure) = failure {
                        self.unpublish(generation);
                        self.record_error(format!(
                            "MCP server `{server_name}` background liveness probe failed: {failure}"
                        ));
                        let shutdown = connection.cancel_and_reap(self, &server_name).await;
                        return if shutdown {
                            ConnectionExit::Shutdown
                        } else {
                            ConnectionExit::Disconnected
                        };
                    }
                }
            }
        }
    }

    async fn reap_child(&self, child: Option<StdioChildGuard>) {
        reap_child(
            self.entry.clone(),
            Arc::clone(&self.active_pid),
            child,
            self.shutdown_policy,
        )
        .await;
    }

    fn unpublish(&self, generation: u64) {
        if self.current_generation() == Some(generation) {
            self.published.send_replace(None);
            if let Some(entry) = self.entry.upgrade()
                && !entry.is_shutting_down()
            {
                self.set_health(McpServerHealth::Reconnecting {
                    last_error: self.health_error(),
                });
            }
        }
    }

    fn current_generation(&self) -> Option<u64> {
        self.published
            .borrow()
            .as_ref()
            .map(|service| service.generation)
    }

    fn schedule_reconnect(&mut self) {
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        if entry.is_shutting_down() {
            return;
        }
        if entry.config.reconnect_max_attempts() == crate::ReconnectAttempts::Disabled {
            self.set_health(McpServerHealth::Disconnected {
                last_error: self.health_error(),
            });
            tracing::info!(server = %entry.server_name, "MCP automatic reconnect is disabled");
            return;
        }
        if !entry
            .config
            .reconnect_max_attempts()
            .allows(self.reconnect_attempts)
        {
            self.record_exhaustion();
            return;
        }
        let jittered = self
            .entry
            .upgrade()
            .map_or(self.reconnect_backoff, |entry| {
                (entry.reconnect_jitter.read_recover())(self.reconnect_backoff)
            });
        let deadline = Instant::now() + jittered;
        self.reconnect_at = Some(deadline);
        self.set_health(McpServerHealth::Reconnecting {
            last_error: self.health_error(),
        });
        #[cfg(test)]
        if let Some(observer) = self
            .entry
            .upgrade()
            .and_then(|entry| entry.lifecycle_observer())
        {
            let _ = observer
                .send(crate::service_lifecycle::LifecycleEvent::ReconnectScheduled { deadline });
        }
    }

    fn advance_reconnect_backoff(&mut self) {
        let max = self
            .entry
            .upgrade()
            .map_or(self.reconnect_backoff, |entry| {
                entry.config.reconnect_max_backoff()
            });
        self.reconnect_backoff = self.reconnect_backoff.saturating_mul(2).min(max);
    }

    fn rearm_exhausted_reconnect(&mut self) {
        if self.reconnect_at.is_none()
            && self.entry.upgrade().is_some_and(|entry| {
                entry.config.reconnect_max_attempts() != crate::ReconnectAttempts::Disabled
                    && matches!(
                        *entry.health.read_recover(),
                        McpServerHealth::Exhausted { .. }
                    )
            })
        {
            self.reconnect_attempts = 0;
            self.schedule_reconnect();
        }
    }

    fn advance_keepalive_deadline(&mut self) {
        let interval = self.entry.upgrade().map_or(Duration::ZERO, |entry| {
            entry.config.liveness_probe_interval()
        });
        self.keepalive_at = (!interval.is_zero()).then(|| Instant::now() + interval);
    }

    fn set_health(&self, health: McpServerHealth) {
        if let Some(entry) = self.entry.upgrade() {
            *entry.health.write_recover() = health;
        }
    }

    fn health_error(&self) -> Option<McpServerFault> {
        self.entry
            .upgrade()
            .and_then(|entry| entry.health.read_recover().fault().cloned())
    }

    fn begin_shutdown(&self) {
        self.set_health(McpServerHealth::ShuttingDown {
            reason: self.health_error(),
        });
    }

    fn record_mcp_error(&self, error: &McpError) {
        let fault = match error {
            McpError::UnusableSchema(source) => {
                McpServerFault::UnusableSchema(Box::new(source.clone()))
            }
            McpError::PoolShutDown
            | McpError::Config(_)
            | McpError::Io(_)
            | McpError::Json(_)
            | McpError::Protocol(_)
            | McpError::StartupTimeout { .. }
            | McpError::Reconfigure(_) => McpServerFault::Connection(error.to_string()),
        };
        self.record_fault(fault);
    }

    fn record_error(&self, error: String) {
        self.record_fault(McpServerFault::Connection(error));
    }

    fn record_fault(&self, error: McpServerFault) {
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        let health = if entry.is_shutting_down() {
            McpServerHealth::ShuttingDown {
                reason: Some(error),
            }
        } else if self.current_generation().is_some() {
            McpServerHealth::Connected {
                catalog_error: Some(error),
            }
        } else {
            McpServerHealth::Reconnecting {
                last_error: Some(error),
            }
        };
        self.set_health(health);
    }

    fn record_exhaustion(&self) {
        self.set_health(McpServerHealth::Exhausted {
            attempts: self.reconnect_attempts,
            last_error: self.health_error(),
        });
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        tracing::warn!(server = %entry.server_name, attempts = self.reconnect_attempts, "MCP reconnect attempts exhausted");
        #[cfg(test)]
        if let Some(observer) = entry.lifecycle_observer() {
            let _ = observer.send(crate::service_lifecycle::LifecycleEvent::ReconnectExhausted);
        }
    }

    #[cfg(test)]
    async fn pause_mid_establish(&mut self) -> bool {
        let hook = self
            .entry
            .upgrade()
            .and_then(|entry| entry.mid_establish_hook.read_recover().clone());
        let Some(hook) = hook else {
            return false;
        };
        hook.reached.notify_one();
        loop {
            tokio::select! {
                () = hook.release.notified() => return false,
                command = self.commands.recv() => match Self::reduce_command(
                    CommandPhase::TestPause,
                    command,
                    "",
                ) {
                    CommandAction::Shutdown => {
                        self.begin_shutdown();
                        return true;
                    },
                    CommandAction::Continue => {}
                    _ => unreachable!("test-pause reducer returned an active action"),
                }
            }
        }
    }

    #[cfg(not(test))]
    async fn pause_mid_establish(&mut self) -> bool {
        false
    }

    #[cfg(test)]
    fn maybe_panic_on_service_quit(&self) {
        if self
            .entry
            .upgrade()
            .is_some_and(|entry| entry.panic_actor_on_quit.load(Ordering::SeqCst))
        {
            panic!("injected MCP lifecycle actor panic");
        }
    }

    #[cfg(not(test))]
    fn maybe_panic_on_service_quit(&self) {}

    #[cfg(test)]
    async fn wedge_shutdown_if_injected(&self) {
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        let pid = entry.shutdown_wedge_pid.load(Ordering::SeqCst);
        if pid != 0 {
            self.active_pid.store(pid, Ordering::SeqCst);
            if let Some(observer) = entry.lifecycle_observer() {
                let _ = observer.send(crate::service_lifecycle::LifecycleEvent::Wedged { pid });
            }
            drop(entry);
            pending::<()>().await;
        }
    }

    #[cfg(not(test))]
    async fn wedge_shutdown_if_injected(&self) {}
}

impl Drop for LifecycleActor {
    fn drop(&mut self) {
        self.published.send_replace(None);
        if self.finished {
            self.begin_shutdown();
            return;
        }
        let pid = self.active_pid.load(Ordering::SeqCst);
        let bound = self.shutdown_policy.total_bound();
        let reason = if std::thread::panicking() {
            "MCP lifecycle actor terminated with JoinError: actor panicked".to_string()
        } else if pid == 0 {
            format!(
                "MCP lifecycle actor abandoned: it did not finish within the {bound:?} per-entry total shutdown deadline"
            )
        } else {
            format!(
                "MCP stdio child PID {pid} abandoned: lifecycle actor did not finish within the {bound:?} per-entry total shutdown deadline"
            )
        };
        self.set_health(McpServerHealth::ShuttingDown {
            reason: Some(McpServerFault::Connection(reason)),
        });
    }
}

async fn reap_child(
    entry: Weak<McpEntry>,
    active_pid: Arc<AtomicU32>,
    child: Option<StdioChildGuard>,
    shutdown_policy: McpShutdownPolicy,
) {
    let Some(child) = child else {
        active_pid.store(0, Ordering::SeqCst);
        return;
    };
    #[cfg(test)]
    let mut child = child;
    #[cfg(test)]
    if let Some(observer) = entry.upgrade().and_then(|entry| entry.lifecycle_observer()) {
        child.observe(observer);
    }
    let pid = child.pid();
    if let Err(error) = child
        .reap_after_graceful_close(
            shutdown_policy.graceful_period,
            shutdown_policy.post_kill_wait,
            shutdown_policy.child_exit_poll_interval,
        )
        .await
    {
        let reason = format!(
            "MCP stdio child PID {pid} abandoned unreaped after bounded lifecycle cleanup: {error}"
        );
        if let Some(entry) = entry.upgrade() {
            let mut health = entry.health.write_recover();
            // Attempt cleanup does not request entry shutdown. Preserve the
            // connection fault and let its configured reconnect policy settle
            // health; only an actual shutdown retains its cleanup failure here.
            if health.is_shutting_down() {
                *health = McpServerHealth::ShuttingDown {
                    reason: Some(McpServerFault::Connection(reason)),
                };
            }
        }
        // StdioChildGuard reports the unreaped child when it drops.
    }
    active_pid.store(0, Ordering::SeqCst);
}

async fn shutdown_http_connection(
    request_tasks: Arc<crate::host::McpHostRequestTasks>,
    waiting: ServiceWaiting,
    graceful_period: Duration,
) {
    let (_, ()) = tokio::join!(
        waiting.wait_for_cleanup(graceful_period),
        request_tasks.shutdown(),
    );
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending().await,
    }
}

#[cfg(test)]
async fn pause_probe_select_if_injected(entry: &McpEntry) {
    let hook = entry.probe_select_hook.write_recover().take();
    if let Some(hook) = hook {
        hook.reached.notify_one();
        hook.release.notified().await;
    }
}

fn send_result(reply: Option<oneshot::Sender<Result<(), McpError>>>, result: Result<(), McpError>) {
    if let Some(reply) = reply {
        let _ = reply.send(result);
    }
}

fn send_shutdown(reply: Option<oneshot::Sender<Result<(), McpError>>>) {
    send_result(reply, Err(McpError::PoolShutDown));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn completed_http_wait_still_shuts_down_host_tasks_without_repoll() {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed_polls = Arc::clone(&polls);
        let future = std::future::poll_fn(move |_| {
            assert_eq!(
                observed_polls.fetch_add(1, Ordering::SeqCst),
                0,
                "ordinary async waiting future was polled after completion"
            );
            std::task::Poll::Ready(Ok(QuitReason::Closed))
        });
        let mut waiting = ServiceWaiting::new(Box::pin(future));

        assert!(matches!((&mut waiting).await, Ok(QuitReason::Closed)));
        shutdown_http_connection(
            Arc::new(crate::host::McpHostRequestTasks::default()),
            waiting,
            Duration::from_secs(3),
        )
        .await;

        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }
}
