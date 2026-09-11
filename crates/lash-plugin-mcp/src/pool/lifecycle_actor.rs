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

use lash_sansio::sync::RwLockExt;
use rmcp::service::QuitReason;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};

use super::{McpEntry, McpToolListRefresh, PublishedService, import_tools};
use crate::config::McpShutdownPolicy;
use crate::error::McpError;
use crate::service_lifecycle::{ConnectingService, StdioChildGuard, connect_service};

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
        reply: oneshot::Sender<Option<String>>,
    },
    InstallToolCatalog {
        generation: u64,
        tools: Vec<rmcp::model::Tool>,
    },
    Shutdown,
}

pub(super) struct LifecycleActor {
    entry: Weak<McpEntry>,
    commands: mpsc::UnboundedReceiver<LifecycleCommand>,
    published: watch::Sender<Option<Arc<PublishedService>>>,
    active_pid: Arc<AtomicU32>,
    shutdown_policy: McpShutdownPolicy,
    generation: u64,
    reconnect_backoff: Duration,
    reconnect_attempts: u64,
    reconnect_at: Option<Instant>,
    keepalive_at: Option<Instant>,
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
        reply: oneshot::Sender<Option<String>>,
    },
    InstallToolCatalog {
        tools: Vec<rmcp::model::Tool>,
    },
    Shutdown,
    Continue,
}

type WaitingFuture =
    Pin<Box<dyn Future<Output = Result<QuitReason, tokio::task::JoinError>> + Send + 'static>>;

struct Connection {
    cancellation: rmcp::service::RunningServiceCancellationToken,
    request_tasks: Arc<crate::host::McpHostRequestTasks>,
    waiting: WaitingFuture,
    child: Option<StdioChildGuard>,
}

impl Connection {
    // Cooperative terminal paths consume this owner and await cleanup. If the
    // actor itself is aborted, dropping the child guard retains the pool's
    // documented forced-abandonment kill-and-log fallback.
    async fn cancel_and_reap(mut self, actor: &mut LifecycleActor, server_name: &str) -> bool {
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
                    reap_child(entry, active_pid, server_name, self.child, shutdown_policy,),
                );
            } else {
                // HTTP has no child to reap, but still gets the configured
                // grace for its transport task to drain.
                let (_, _) = tokio::join!(
                    self.request_tasks.shutdown(),
                    timeout(shutdown_policy.graceful_period, self.waiting.as_mut()),
                );
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
            LifecycleCommand::InstallToolCatalog { generation, tools } => {
                if phase.observes(generation) {
                    CommandAction::InstallToolCatalog { tools }
                } else {
                    CommandAction::Continue
                }
            }
            LifecycleCommand::Shutdown => CommandAction::Shutdown,
        }
    }

    pub(super) fn new(
        entry: Weak<McpEntry>,
        commands: mpsc::UnboundedReceiver<LifecycleCommand>,
        published: watch::Sender<Option<Arc<PublishedService>>>,
        active_pid: Arc<AtomicU32>,
        shutdown_policy: McpShutdownPolicy,
        reconnect_initial_backoff: Duration,
        keepalive_interval: Duration,
    ) -> Self {
        Self {
            entry,
            commands,
            published,
            active_pid,
            shutdown_policy,
            generation: 0,
            reconnect_backoff: reconnect_initial_backoff,
            reconnect_attempts: 0,
            reconnect_at: None,
            keepalive_at: (!keepalive_interval.is_zero())
                .then(|| Instant::now() + keepalive_interval),
        }
    }

    pub(super) async fn run(mut self) {
        loop {
            let reconnect_at = self.reconnect_at;
            let keepalive_at = self.keepalive_at;
            tokio::select! {
                command = self.commands.recv() => {
                    match Self::reduce_command(CommandPhase::Idle, command, "") {
                        CommandAction::Establish { reply } => {
                            self.reconnect_at = None;
                            self.reconnect_attempts = 0;
                            self.set_reconnect_exhausted(false);
                            if matches!(self.connect_and_run(Some(reply)).await, ConnectionExit::Shutdown) {
                                return;
                            }
                            self.schedule_reconnect();
                        }
                        CommandAction::Shutdown => {
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
                            let max_attempts = self.entry.upgrade()
                                .map_or(0, |entry| entry.config.reconnect_max_attempts());
                            if max_attempts != 0 && self.reconnect_attempts >= max_attempts {
                                self.set_reconnect_exhausted(true);
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
        let shutdown_requested = Arc::clone(&entry.shutting_down);
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
                self.record_error(error.to_string());
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
                        drop(connection_attempt);
                        if let Some(pid) = stdio_child.as_ref().map(StdioChildGuard::pid) {
                            self.record_error(format!(
                                "MCP stdio child PID {pid} handshake interrupted by pool shutdown"
                            ));
                        }
                        self.reap_child(&server_name, stdio_child.take()).await;
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
                self.record_error(error.to_string());
                self.reap_child(&server_name, stdio_child.take()).await;
                send_result(initial_reply, Err(error));
                return ConnectionExit::Failed;
            }
            Err(_) => {
                drop(connection_attempt);
                let error = McpError::StartupTimeout {
                    server: server_name.clone(),
                    timeout_ms: startup_timeout.as_millis() as u64,
                };
                self.record_error(error.to_string());
                self.reap_child(&server_name, stdio_child.take()).await;
                send_result(initial_reply, Err(error));
                return ConnectionExit::Failed;
            }
        };

        let peer = running.peer().clone();
        let mut connection = Connection {
            cancellation: running.cancellation_token(),
            request_tasks: running.service().request_tasks(),
            waiting: Box::pin(running.waiting()),
            child: stdio_child.take(),
        };
        if self.pause_mid_establish().await {
            connection.cancel_and_reap(self, &server_name).await;
            send_shutdown(initial_reply);
            return ConnectionExit::Shutdown;
        }

        let discovery = timeout(startup_timeout, peer.list_all_tools());
        tokio::pin!(discovery);
        let tools = loop {
            tokio::select! {
                biased;
                result = &mut discovery => {
                    break match result {
                        Ok(Ok(tools)) => tools,
                        Ok(Err(error)) => {
                            let error = McpError::Protocol(format!("list_tools failed: {error}"));
                            self.record_error(error.to_string());
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
                            self.record_error(error.to_string());
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
                reason = connection.waiting.as_mut() => {
                    let cause = format!("MCP server `{server_name}` service quit during discovery: {reason:?}");
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
                        connection.cancel_and_reap(self, &server_name).await;
                        send_shutdown(initial_reply);
                        return ConnectionExit::Shutdown;
                    }
                    CommandAction::Continue => {}
                    _ => unreachable!("discovery command reducer returned an active action"),
                }
            }
        };

        let imported = match import_tools(&server_name, tools) {
            Ok(imported) => imported,
            Err(error) => {
                self.record_error(error.to_string());
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
            self.record_error(error.to_string());
            let shutdown = connection.cancel_and_reap(self, &server_name).await;
            if shutdown {
                send_shutdown(initial_reply);
                return ConnectionExit::Shutdown;
            }
            send_result(initial_reply, Err(error));
            return ConnectionExit::Failed;
        }
        entry.consecutive_timeouts.store(0, Ordering::SeqCst);
        *entry.last_error.write_recover() = None;
        entry.reconnect_exhausted.store(false, Ordering::SeqCst);
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
                reason = connection.waiting.as_mut() => {
                    let cause = format!("MCP server `{server_name}` service quit: {reason:?}");
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
                        self.unpublish(generation);
                        let _ = connection.cancel_and_reap(self, &server_name).await;
                        return ConnectionExit::Shutdown;
                    }
                    CommandAction::CallSucceeded => {
                        if let Some(entry) = self.entry.upgrade() {
                            entry.consecutive_timeouts.store(0, Ordering::SeqCst);
                            *entry.last_error.write_recover() = None;
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
                        let _ = reply.send(Some(cause.clone()));
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
                    CommandAction::InstallToolCatalog { tools } => {
                        if let Some(entry) = self.entry.upgrade()
                            && let Err(error) = import_tools(&server_name, tools)
                                .and_then(|imported| entry.replace_imported_tools(imported))
                        {
                            tracing::warn!(
                                server = %server_name,
                                error = %error,
                                "MCP tools/list refresh refused"
                            );
                            self.record_error(error.to_string());
                        }
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
                            tokio::select! {
                                biased;
                                reason = connection.waiting.as_mut() => {
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
                                        *entry.last_error.write_recover() = None;
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
                                        let _ = reply.send(Some(cause.clone()));
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
                                    CommandAction::InstallToolCatalog { tools } => {
                                        if let Err(error) = import_tools(&server_name, tools)
                                            .and_then(|imported| entry.replace_imported_tools(imported))
                                        {
                                            tracing::warn!(
                                                server = %server_name,
                                                error = %error,
                                                "MCP tools/list refresh refused"
                                            );
                                            self.record_error(error.to_string());
                                        }
                                    }
                                    CommandAction::Continue => {}
                                    CommandAction::Establish { .. } => {
                                        unreachable!("probe reducer returned an establish action")
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
                            }
                        }
                    };
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

    async fn reap_child(&self, server_name: &str, child: Option<StdioChildGuard>) {
        reap_child(
            self.entry.clone(),
            Arc::clone(&self.active_pid),
            server_name,
            child,
            self.shutdown_policy,
        )
        .await;
    }

    fn unpublish(&self, generation: u64) {
        if self.current_generation() == Some(generation) {
            self.published.send_replace(None);
        }
    }

    fn current_generation(&self) -> Option<u64> {
        self.published
            .borrow()
            .as_ref()
            .map(|service| service.generation)
    }

    fn schedule_reconnect(&mut self) {
        let jittered = self
            .entry
            .upgrade()
            .map_or(self.reconnect_backoff, |entry| {
                (entry.reconnect_jitter.read_recover())(self.reconnect_backoff)
            });
        self.reconnect_at = Some(Instant::now() + jittered);
        self.set_reconnect_exhausted(false);
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
        if self.reconnect_at.is_none() && self.is_reconnect_exhausted() {
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

    fn record_error(&self, error: String) {
        if let Some(entry) = self.entry.upgrade() {
            *entry.last_error.write_recover() = Some(error);
        }
    }

    fn set_reconnect_exhausted(&self, exhausted: bool) {
        if let Some(entry) = self.entry.upgrade() {
            entry.reconnect_exhausted.store(exhausted, Ordering::SeqCst);
        }
    }

    fn is_reconnect_exhausted(&self) -> bool {
        self.entry
            .upgrade()
            .is_some_and(|entry| entry.reconnect_exhausted.load(Ordering::SeqCst))
    }

    fn record_exhaustion(&self) {
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        let previous = entry
            .last_error
            .read_recover()
            .clone()
            .unwrap_or_else(|| "unknown connection error".to_string());
        *entry.last_error.write_recover() = Some(format!(
            "MCP server `{}` reconnect attempts exhausted after {} attempt(s); no background recovery is active; last error: {previous}",
            entry.server_name, self.reconnect_attempts
        ));
        tracing::warn!(
            server = %entry.server_name,
            attempts = self.reconnect_attempts,
            "MCP reconnect attempts exhausted"
        );
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
                    CommandAction::Shutdown => return true,
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

async fn reap_child(
    entry: Weak<McpEntry>,
    active_pid: Arc<AtomicU32>,
    server_name: &str,
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
        )
        .await
    {
        let reason = format!(
            "MCP stdio child PID {pid} abandoned unreaped after bounded lifecycle cleanup: {error}"
        );
        if let Some(entry) = entry.upgrade() {
            *entry.last_error.write_recover() = Some(reason.clone());
        }
        tracing::error!(server = %server_name, pid, reason = %reason, "MCP lifecycle actor abandoned a stdio child");
    }
    active_pid.store(0, Ordering::SeqCst);
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending().await,
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
