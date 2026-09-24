use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use http::{HeaderName, HeaderValue};
use rmcp::ServiceError;
use rmcp::service::{RoleClient, RunningService, RxJsonRpcMessage, ServiceExt, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use tokio::time::Instant;

use crate::config::{McpServerConfig, McpStdioTransport, McpTransport};
use crate::error::McpError;
use crate::host::{LashMcpClientHandler, McpHostServices, McpToolListChangedHandler};

struct ManagedChildTransport {
    io: AsyncRwTransport<RoleClient, tokio::fs::File, tokio::fs::File>,
}

impl Transport<RoleClient> for ManagedChildTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.io.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.io.receive()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.io.close()
    }
}

type HandshakeFuture = Pin<
    Box<
        dyn Future<Output = Result<RunningService<RoleClient, LashMcpClientHandler>, McpError>>
            + Send,
    >,
>;

pub(crate) struct ConnectingService {
    pub(crate) handshake: HandshakeFuture,
    pub(crate) stdio_child: Option<StdioChildGuard>,
}

pub(crate) fn connect_service(
    server_name: &str,
    config: &McpServerConfig,
    host_services: McpHostServices,
    tool_list_changed: Arc<dyn McpToolListChangedHandler>,
    active_pid: Arc<AtomicU32>,
    shutdown_requested: Arc<AtomicBool>,
) -> Result<ConnectingService, McpError> {
    let client_handler = LashMcpClientHandler::new(server_name, host_services)
        .with_tool_list_changed_handler(tool_list_changed);

    match &config.transport {
        McpTransport::Stdio(transport) => {
            let command = &transport.command;
            let child = spawn_stdio_server(server_name, transport)?;
            // Preparation errors are returned by the handshake future so the actor first takes
            // ownership of the exact child handle and can always reap it.
            let mut stdio_child = StdioChildGuard::new(server_name, child, shutdown_requested);
            active_pid.store(stdio_child.pid(), Ordering::SeqCst);
            let io = match (
                stdio_child.child.stdout.take(),
                stdio_child.child.stdin.take(),
            ) {
                (Some(stdout), Some(stdin)) => Ok(ManagedChildTransport {
                    io: AsyncRwTransport::new(
                        tokio::fs::File::from_std(child_stdout_file(stdout)),
                        tokio::fs::File::from_std(child_stdin_file(stdin)),
                    ),
                }),
                (None, _) => Err(McpError::Protocol(format!(
                    "failed to capture stdout for `{command}` MCP server `{server_name}`"
                ))),
                (_, None) => Err(McpError::Protocol(format!(
                    "failed to capture stdin for `{command}` MCP server `{server_name}`"
                ))),
            };
            let server_name = server_name.to_string();
            let handshake = Box::pin(async move {
                let transport = io?;
                client_handler.serve(transport).await.map_err(|err| {
                    McpError::Protocol(format!("MCP handshake with `{server_name}`: {err}"))
                })
            });
            Ok(ConnectingService {
                handshake,
                stdio_child: Some(stdio_child),
            })
        }
        McpTransport::StreamableHttp(transport) => {
            let url = &transport.url;
            let headers = &transport.headers;
            active_pid.store(0, Ordering::SeqCst);
            let custom_headers = build_http_headers(server_name, headers)?;
            let config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                .custom_headers(custom_headers);
            let transport = StreamableHttpClientTransport::from_config(config);
            let server_name = server_name.to_string();
            let handshake = Box::pin(async move {
                client_handler.serve(transport).await.map_err(|err| {
                    McpError::Protocol(format!("MCP handshake with `{server_name}`: {err}"))
                })
            });
            Ok(ConnectingService {
                handshake,
                stdio_child: None,
            })
        }
    }
}

/// Spawns the configured stdio server. On Unix the child leads its own
/// process group (`setpgid(0, 0)` via
/// [`std::os::unix::process::CommandExt::process_group`]), so the
/// forced-shutdown path can signal the whole group — grandchildren spawned
/// through `npx`/`uvx` wrappers included — instead of the bare pid.
#[expect(
    clippy::disallowed_methods,
    reason = "spawning the configured MCP stdio server is this plugin's purpose; the host supplies the command (FIG-2971)"
)]
fn spawn_stdio_server(
    server_name: &str,
    transport: &McpStdioTransport,
) -> Result<std::process::Child, McpError> {
    let command = &transport.command;
    let mut cmd = std::process::Command::new(command);
    cmd.args(&transport.args);
    if let Some(cwd) = &transport.cwd {
        cmd.current_dir(cwd);
    }
    for (key, value) in &transport.env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|err| {
        McpError::Protocol(format!(
            "failed to spawn `{command}` for `{server_name}`: {err}"
        ))
    })
}

/// Signals the child's whole process group when the child leads one — the
/// spawn path makes every stdio server its own group leader — and falls back
/// to the bare pid otherwise, so a guard wrapped around an ungrouped child
/// can never signal the host's own group. `ESRCH` means the process is
/// already gone and reports success: the shutdown goal is met either way.
#[cfg(unix)]
#[expect(
    unsafe_code,
    reason = "terminating an MCP stdio server's process group needs kill(2) and getpgid(2); libc is the narrowest FFI for both (FIG-3519)"
)]
fn signal_child_process_group(pid: u32, signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: kill(2) and getpgid(2) are syscalls with no memory-safety
    // contract. A negative target reaches the process group only when the
    // child verifiably leads it, so an ungrouped child never lets a group
    // signal hit the host's own process group.
    let target = unsafe {
        if libc::getpgid(pid as i32) == pid as i32 {
            -(pid as i32)
        } else {
            pid as i32
        }
    };
    if unsafe { libc::kill(target, signal) } == -1 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        };
    }
    Ok(())
}

#[cfg(unix)]
fn child_stdout_file(stdout: std::process::ChildStdout) -> std::fs::File {
    let stdout: std::os::fd::OwnedFd = stdout.into();
    stdout.into()
}

#[cfg(unix)]
fn child_stdin_file(stdin: std::process::ChildStdin) -> std::fs::File {
    let stdin: std::os::fd::OwnedFd = stdin.into();
    stdin.into()
}

#[cfg(windows)]
fn child_stdout_file(stdout: std::process::ChildStdout) -> std::fs::File {
    let stdout: std::os::windows::io::OwnedHandle = stdout.into();
    stdout.into()
}

#[cfg(windows)]
fn child_stdin_file(stdin: std::process::ChildStdin) -> std::fs::File {
    let stdin: std::os::windows::io::OwnedHandle = stdin.into();
    stdin.into()
}

/// Translate configured headers into the types rmcp's HTTP transport expects.
pub(crate) fn build_http_headers(
    server_name: &str,
    headers: &BTreeMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, McpError> {
    let mut out = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let header_name = HeaderName::try_from(name.as_str()).map_err(|err| {
            McpError::Config(format!(
                "MCP server `{server_name}` has invalid HTTP header name `{name}`: {err}"
            ))
        })?;
        let header_value = HeaderValue::try_from(value.as_str()).map_err(|err| {
            McpError::Config(format!(
                "MCP server `{server_name}` has invalid value for HTTP header `{name}`: {err}"
            ))
        })?;
        out.insert(header_name, header_value);
    }
    Ok(out)
}

/// Transport-level failures mean the connection is gone (dead child process,
/// closed HTTP stream). Protocol-level errors leave the connection usable.
pub(crate) fn is_connection_loss(error: &ServiceError) -> bool {
    match error {
        ServiceError::TransportSend(_) | ServiceError::TransportClosed => true,
        ServiceError::McpError(_)
        | ServiceError::UnexpectedResponse
        | ServiceError::Cancelled { .. }
        | ServiceError::Timeout { .. } => false,
        _ => true,
    }
}

pub(crate) fn equal_jitter(max: std::time::Duration) -> std::time::Duration {
    let max_ms = u64::try_from(max.as_millis()).unwrap_or(u64::MAX);
    let min_ms = max_ms.saturating_add(1) / 2;
    std::time::Duration::from_millis(fastrand::u64(min_ms..=max_ms))
}

/// Lifecycle transitions of one actor-owned child, reported to the owning
/// entry's test observer. Deadlines are the runtime clock's instants, so a
/// test that owns a paused clock can cross them exactly.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleEvent {
    /// The actor took ownership of a freshly spawned stdio child.
    Spawned { pid: u32 },
    /// Graceful reaping began; forced termination begins at `deadline` unless
    /// the child exits first.
    GraceArmed { pid: u32, deadline: Instant },
    /// The terminate request was sent to the child's process group; the group
    /// is hard-killed at `deadline` unless it exits first.
    TermIssued { pid: u32, deadline: Instant },
    /// The kill request was sent to the child's process group; the child is
    /// abandoned at `deadline` unless it exits first.
    KillIssued { pid: u32, deadline: Instant },
    /// The child exited and was reaped.
    Reaped { pid: u32 },
    /// The guard dropped without reaping the child (killed, never waited).
    Abandoned { pid: u32 },
    /// An injected wedge holds the actor forever after a shutdown request.
    Wedged { pid: u32 },
    /// The bounded reconnect loop spent its final attempt.
    ReconnectExhausted,
    /// The actor armed its next reconnect attempt for `deadline`.
    ReconnectScheduled { deadline: Instant },
}

#[cfg(test)]
pub(crate) type LifecycleObserver = tokio::sync::mpsc::UnboundedSender<LifecycleEvent>;

/// Wakes the reaper whenever a child of this process changes state, so a
/// child exit is observed as a rendezvous rather than by polling. Without a
/// SIGCHLD stream (non-unix targets, or a runtime built without the signal
/// driver) the reaper polls on the runtime clock, which stays the deadline's
/// clock.
enum ChildExits {
    #[cfg(unix)]
    Signal(tokio::signal::unix::Signal),
    Polled,
}

impl ChildExits {
    fn subscribe() -> Self {
        #[cfg(unix)]
        if let Some(signal) = child_signal_stream() {
            return Self::Signal(signal);
        }
        Self::Polled
    }

    async fn recv(&mut self) {
        match self {
            #[cfg(unix)]
            Self::Signal(signal) => {
                if signal.recv().await.is_none() {
                    // The signal driver is gone; only the deadline can end the wait.
                    std::future::pending::<()>().await;
                }
            }
            Self::Polled => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

#[cfg(unix)]
thread_local! {
    /// Set while this thread probes for the SIGCHLD stream, so the probe's
    /// own panic never reaches the process panic hook.
    static PROBING_CHILD_SIGNAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Subscribes to SIGCHLD on the current runtime, or reports why it cannot.
///
/// Tokio panics rather than errs when the runtime has no signal driver (a
/// runtime built without `enable_io`) or when no runtime is entered, so the
/// subscription is probed under `catch_unwind` with the panic hook silenced
/// for this thread; the probe's failure is a warning, never a panic.
#[cfg(unix)]
fn child_signal_stream() -> Option<tokio::signal::unix::Signal> {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    // `catch_unwind` cannot contain a panic under `panic = "abort"`; the
    // probe below would abort the host, so poll on the runtime clock instead.
    if cfg!(panic = "abort") {
        return None;
    }

    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !PROBING_CHILD_SIGNAL.with(std::cell::Cell::get) {
                previous(info);
            }
        }));
    });

    PROBING_CHILD_SIGNAL.with(|probing| probing.set(true));
    let probed = catch_unwind(AssertUnwindSafe(|| {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
    }));
    PROBING_CHILD_SIGNAL.with(|probing| probing.set(false));
    match probed {
        Ok(Ok(signal)) => Some(signal),
        Ok(Err(error)) => {
            tracing::warn!(
                error = %error,
                "MCP stdio reaper cannot subscribe to SIGCHLD; polling on the runtime clock"
            );
            None
        }
        Err(panic) => {
            let reason = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("signal driver unavailable");
            tracing::warn!(
                reason,
                "MCP stdio reaper has no signal driver; polling on the runtime clock"
            );
            None
        }
    }
}

/// Exact child-process handle retained outside rmcp's async service task.
///
/// Explicit shutdown closes the child's stdin, gives it a grace period, then
/// escalates SIGTERM then SIGKILL to the child's process group — the spawn
/// path makes every stdio server a group leader, so the signals reach
/// grandchildren a `npx`/`uvx` wrapper leaves behind — and waits to reap it.
/// Dropping the guard without that shutdown sends the same escalation back to
/// back and logs: waiting in `Drop` cannot be made reliably bounded.
///
/// Reap deadlines are runtime-clock instants (`tokio::time`), the same clock
/// the lifecycle actor's own timers use; child exits arrive as a SIGCHLD
/// rendezvous. Nothing here reads wall-clock time.
pub(crate) struct StdioChildGuard {
    server_name: String,
    pid: u32,
    child: std::process::Child,
    reaped: bool,
    explicit_abandonment: bool,
    shutdown_requested: Arc<AtomicBool>,
    #[cfg(test)]
    never_finish_reap: bool,
    #[cfg(test)]
    observer: Option<LifecycleObserver>,
}

impl StdioChildGuard {
    pub(crate) fn new(
        server_name: &str,
        child: std::process::Child,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Self {
        Self {
            server_name: server_name.to_string(),
            pid: child.id(),
            child,
            reaped: false,
            explicit_abandonment: false,
            shutdown_requested,
            #[cfg(test)]
            never_finish_reap: false,
            #[cfg(test)]
            observer: None,
        }
    }

    pub(crate) async fn reap_after_graceful_close(
        mut self,
        graceful_period: Duration,
        post_kill_wait: Duration,
    ) -> std::io::Result<()> {
        self.explicit_abandonment = true;
        let mut exits = ChildExits::subscribe();
        let deadline = Instant::now() + graceful_period;
        #[cfg(test)]
        self.emit(LifecycleEvent::GraceArmed {
            pid: self.pid,
            deadline,
        });
        if self.exited_by(&mut exits, deadline).await? {
            return Ok(());
        }

        // A well-behaved server exits on SIGTERM; the same bound then separates
        // the terminate request from the kill it precedes.
        let term_error = self.terminate().err();
        let term_deadline = Instant::now() + post_kill_wait;
        #[cfg(test)]
        self.emit(LifecycleEvent::TermIssued {
            pid: self.pid,
            deadline: term_deadline,
        });
        if self.exited_by(&mut exits, term_deadline).await? {
            return Ok(());
        }

        let kill_error = self.force_kill().err();
        let reap_deadline = Instant::now() + post_kill_wait;
        #[cfg(test)]
        self.emit(LifecycleEvent::KillIssued {
            pid: self.pid,
            deadline: reap_deadline,
        });
        if self.exited_by(&mut exits, reap_deadline).await? {
            return Ok(());
        }
        let kill_context = [term_error, kill_error]
            .into_iter()
            .flatten()
            .map(|error| format!("; termination request failed: {error}"))
            .collect::<String>();
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "MCP stdio child PID {} did not exit within {post_kill_wait:?} after the kill request{kill_context}",
                self.pid,
            ),
        ))
    }

    /// Reaps the child if it exits before `deadline` passes on the runtime
    /// clock. `Ok(false)` means the deadline passed with the child still
    /// running.
    async fn exited_by(
        &mut self,
        exits: &mut ChildExits,
        deadline: Instant,
    ) -> std::io::Result<bool> {
        let expiry = tokio::time::sleep_until(deadline);
        tokio::pin!(expiry);
        loop {
            if self.try_wait()?.is_some() {
                self.reaped = true;
                #[cfg(test)]
                self.emit(LifecycleEvent::Reaped { pid: self.pid });
                return Ok(true);
            }
            tokio::select! {
                biased;
                () = exits.recv() => {}
                () = &mut expiry => return Ok(false),
            }
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// SIGTERM to the child's process group — or the bare pid when the child
    /// leads no group — asking the whole server tree to exit.
    fn terminate(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            signal_child_process_group(self.pid, libc::SIGTERM)
        }
        #[cfg(not(unix))]
        {
            self.child.kill()
        }
    }

    /// SIGKILL to the child's process group — or the bare pid when the child
    /// leads no group. SIGKILL cannot be trapped, so it always lands.
    fn force_kill(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            signal_child_process_group(self.pid, libc::SIGKILL)
        }
        #[cfg(not(unix))]
        {
            self.child.kill()
        }
    }

    pub(crate) fn begin_bounded_cleanup(&mut self) {
        self.explicit_abandonment = true;
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        #[cfg(test)]
        if self.never_finish_reap {
            return Ok(None);
        }
        self.child.try_wait()
    }

    #[cfg(test)]
    pub(crate) fn never_finish_reap(&mut self) {
        self.never_finish_reap = true;
    }

    /// Reports this child's lifecycle transitions to the owning entry's
    /// observer.
    #[cfg(test)]
    pub(crate) fn observe(&mut self, observer: LifecycleObserver) {
        self.observer = Some(observer);
    }

    #[cfg(test)]
    fn emit(&self, event: LifecycleEvent) {
        if let Some(observer) = &self.observer {
            let _ = observer.send(event);
        }
    }
}

impl Drop for StdioChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            // Drop cannot interpose a bounded wait, so the same TERM-then-KILL
            // escalation goes to the process group back to back — on Unix this
            // is effectively the group SIGKILL, which is what must not miss.
            let _ = self.terminate();
            let _ = self.force_kill();
            if self.explicit_abandonment || self.shutdown_requested.load(Ordering::SeqCst) {
                tracing::error!(
                    pid = self.pid,
                    server = %self.server_name,
                    "MCP stdio child abandoned unreaped after bounded lifecycle cleanup"
                );
            } else {
                tracing::error!(
                    pid = self.pid,
                    server = %self.server_name,
                    "MCP stdio child killed without explicit pool shutdown; call shutdown_all() to reap it"
                );
            }
            #[cfg(test)]
            self.emit(LifecycleEvent::Abandoned { pid: self.pid });
        }
    }
}

/// Real-clock witnesses: the default clock is unpaused runtime time, so the
/// guard's observable sequence under production timing is pinned here
/// without any test clock in play.
#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[expect(
        clippy::expect_used,
        reason = "test support: the fixture must be able to spawn its witness `sh` child; failure here is a broken test environment"
    )]
    fn observed_child(
        script: &str,
    ) -> (
        StdioChildGuard,
        tokio::sync::mpsc::UnboundedReceiver<LifecycleEvent>,
    ) {
        let child = Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn stdio child");
        let mut guard = StdioChildGuard::new("witness", child, Arc::new(AtomicBool::new(false)));
        let (observer, events) = tokio::sync::mpsc::unbounded_channel();
        guard.observe(observer);
        (guard, events)
    }

    fn drain(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<LifecycleEvent>,
    ) -> Vec<LifecycleEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn child_exiting_on_eof_is_reaped_inside_the_grace_period() {
        let (mut guard, mut events) = observed_child("cat >/dev/null");
        let pid = guard.pid();
        guard.child.stdin.take();
        let started = Instant::now();
        guard
            .reap_after_graceful_close(Duration::from_secs(30), Duration::from_secs(30))
            .await
            .expect("child exits on EOF");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(30),
            "EOF exit must not consume the grace period: {elapsed:?}"
        );
        let observed = drain(&mut events);
        assert_eq!(observed.len(), 2, "{observed:?}");
        assert!(
            matches!(observed[0], LifecycleEvent::GraceArmed { pid: armed, deadline }
                if armed == pid && deadline >= started + Duration::from_secs(30))
        );
        assert_eq!(observed[1], LifecycleEvent::Reaped { pid });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_ignoring_child_is_killed_at_the_grace_deadline() {
        let (mut guard, mut events) = observed_child("trap '' TERM; while :; do sleep 1; done");
        let pid = guard.pid();
        guard.child.stdin.take();
        let started = Instant::now();
        guard
            .reap_after_graceful_close(Duration::from_millis(50), Duration::from_millis(100))
            .await
            .expect("kill reaps the child");
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "a child ignoring SIGTERM consumes the grace and post-terminate windows"
        );
        let observed = drain(&mut events);
        assert_eq!(observed.len(), 4, "{observed:?}");
        let LifecycleEvent::GraceArmed {
            deadline: grace, ..
        } = observed[0]
        else {
            panic!("{observed:?}");
        };
        let LifecycleEvent::TermIssued {
            pid: termed,
            deadline: term,
        } = observed[1]
        else {
            panic!("{observed:?}");
        };
        assert_eq!(termed, pid);
        assert!(
            term >= grace + Duration::from_millis(100),
            "the terminate deadline is armed when the grace deadline passes"
        );
        let LifecycleEvent::KillIssued {
            pid: killed,
            deadline: cleanup,
        } = observed[2]
        else {
            panic!("{observed:?}");
        };
        assert_eq!(killed, pid);
        assert!(
            cleanup >= term + Duration::from_millis(100),
            "cleanup deadline is armed after the terminate deadline passes"
        );
        assert_eq!(observed[3], LifecycleEvent::Reaped { pid });
    }

    /// A stdio server that ignores stdin EOF and SIGTERM — and a grandchild
    /// that does the same — must both die to the group SIGKILL. This is the
    /// FIG-3519 leak: a `npx`/`uvx` wrapper is reaped while the server it
    /// spawned keeps running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_stdio_shutdown_terminates_process_group() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let grandchild_pidfile = dir.path().join("grandchild.pid");
        // The grandchild records its own pid and, like its parent, never reads
        // stdin and ignores SIGTERM; only a group-wide SIGKILL ends both.
        let script = format!(
            "sh -c 'echo $$ > \"{0}\"; trap \"\" TERM; while :; do sleep 5; done' & trap '' TERM; while :; do sleep 5; done",
            grandchild_pidfile.display()
        );
        let transport = McpStdioTransport::new("sh", vec!["-c".to_string(), script]);
        let child = spawn_stdio_server("fixture", &transport).expect("spawn fixture server");
        let mut guard = StdioChildGuard::new("fixture", child, Arc::new(AtomicBool::new(false)));
        let pid = guard.pid();
        assert_eq!(
            process_group_of(pid),
            Some(pid),
            "the spawn path must make the stdio server a process-group leader"
        );
        guard.child.stdin.take();

        let deadline = Instant::now() + Duration::from_secs(5);
        let grandchild = loop {
            if let Ok(text) = std::fs::read_to_string(&grandchild_pidfile)
                && let Ok(grandchild) = text.trim().parse::<u32>()
            {
                break grandchild;
            }
            assert!(
                Instant::now() < deadline,
                "grandchild pidfile never appeared"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_ne!(grandchild, pid);

        guard
            .reap_after_graceful_close(Duration::from_millis(50), Duration::from_millis(200))
            .await
            .expect("group kill reaps the fixture server");

        // The grandchild is orphaned when its group dies; whether it lingers
        // as an unreaped zombie is the adoptive reaper's business, so the
        // probe accepts zombie state as dead.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process_exited(grandchild) {
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} outlived its process group"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(process_exited(pid));
    }

    /// `getpgid` of a live child: `Some(pgid)` while the process exists.
    #[expect(
        unsafe_code,
        reason = "test support: getpgid(2) probes the fixture's process-group leadership"
    )]
    fn process_group_of(pid: u32) -> Option<u32> {
        // SAFETY: getpgid(2) is a pure query with no memory-safety contract.
        let pgid = unsafe { libc::getpgid(pid as i32) };
        (pgid >= 0).then_some(pgid as u32)
    }

    /// `kill(pid, 0)` liveness probe; an orphan's zombie counts as dead — its
    /// reaping is the adoptive parent's job, not this shutdown's.
    #[expect(
        unsafe_code,
        reason = "test support: kill(2) signal 0 is the portable liveness probe"
    )]
    fn process_exited(pid: u32) -> bool {
        // SAFETY: kill(2) with signal 0 performs error checking only.
        if unsafe { libc::kill(pid as i32, 0) } == -1 {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        }
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some((_, state)) = stat.rsplit_once(") ")
            && state.starts_with('Z')
        {
            return true;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreapable_child_is_abandoned_at_the_cleanup_deadline() {
        let (mut guard, mut events) = observed_child("cat >/dev/null");
        let pid = guard.pid();
        guard.never_finish_reap();
        let started = Instant::now();
        let error = guard
            .reap_after_graceful_close(Duration::from_millis(20), Duration::from_millis(30))
            .await
            .expect_err("a child the reaper never observes is abandoned");
        assert!(started.elapsed() >= Duration::from_millis(80));
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            format!("MCP stdio child PID {pid} did not exit within 30ms after the kill request")
        );
        let observed = drain(&mut events);
        assert!(
            matches!(
                observed.as_slice(),
                [
                    LifecycleEvent::GraceArmed { .. },
                    LifecycleEvent::TermIssued { .. },
                    LifecycleEvent::KillIssued { .. },
                    LifecycleEvent::Abandoned { pid: abandoned }
                ] if *abandoned == pid
            ),
            "{observed:?}"
        );
        // The guard's Drop killed the child; like the pool-level abandonment
        // scenarios, the unreaped zombie is the documented residue.
    }

    /// A runtime without the signal driver has no SIGCHLD stream: the reaper
    /// must fall back to the runtime-clocked poll and still reap, never panic.
    #[test]
    fn reap_without_a_signal_driver_falls_back_to_the_clocked_poll() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("time-only runtime");
        let (mut guard, mut events) = runtime.block_on(async { observed_child("cat >/dev/null") });
        let pid = guard.pid();
        guard.child.stdin.take();
        runtime
            .block_on(
                guard.reap_after_graceful_close(Duration::from_secs(30), Duration::from_secs(30)),
            )
            .expect("child exits on EOF and is reaped without SIGCHLD");
        let observed = drain(&mut events);
        assert!(
            matches!(
                observed.as_slice(),
                [LifecycleEvent::GraceArmed { .. }, LifecycleEvent::Reaped { pid: reaped }]
                    if *reaped == pid
            ),
            "{observed:?}"
        );
    }
}
