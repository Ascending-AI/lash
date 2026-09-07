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

use crate::config::McpServerConfig;
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

    match config {
        McpServerConfig::Stdio {
            command,
            args,
            env,
            cwd,
            ..
        } => {
            let mut cmd = std::process::Command::new(command);
            cmd.args(args);
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            for (key, value) in env {
                cmd.env(key, value);
            }
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
            let child = cmd.spawn().map_err(|err| {
                McpError::Protocol(format!(
                    "failed to spawn `{command}` for `{server_name}`: {err}"
                ))
            })?;
            // Construct the guard immediately after spawn. Preparation errors
            // are returned by the handshake future so the actor first takes
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
        McpServerConfig::StreamableHttp { url, headers, .. } => {
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
    /// Graceful reaping began; the child is killed at `deadline` unless it
    /// exits first.
    GraceArmed { pid: u32, deadline: Instant },
    /// The kill request was sent; the child is abandoned at `deadline` unless
    /// it exits first.
    KillIssued { pid: u32, deadline: Instant },
    /// The child exited and was reaped.
    Reaped { pid: u32 },
    /// The guard dropped without reaping the child (killed, never waited).
    Abandoned { pid: u32 },
    /// An injected wedge holds the actor forever after a shutdown request.
    Wedged { pid: u32 },
    /// The bounded reconnect loop spent its final attempt.
    ReconnectExhausted,
}

#[cfg(test)]
pub(crate) type LifecycleObserver = tokio::sync::mpsc::UnboundedSender<LifecycleEvent>;

/// Wakes the reaper whenever a child of this process changes state, so a
/// child exit is observed as a rendezvous rather than by polling.
#[cfg(unix)]
struct ChildExits(tokio::signal::unix::Signal);

#[cfg(unix)]
impl ChildExits {
    fn subscribe() -> std::io::Result<Self> {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).map(Self)
    }

    async fn recv(&mut self) {
        if self.0.recv().await.is_none() {
            // The signal driver is gone; only the deadline can end the wait.
            std::future::pending::<()>().await;
        }
    }
}

/// Without SIGCHLD the reaper polls on the runtime clock, which stays the
/// deadline's clock.
#[cfg(not(unix))]
struct ChildExits;

#[cfg(not(unix))]
impl ChildExits {
    fn subscribe() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Exact child-process handle retained outside rmcp's async service task.
///
/// Explicit shutdown closes the child's stdin, gives it a grace period, and
/// waits to reap it. Dropping the guard without that shutdown only kills and
/// logs: waiting in `Drop` cannot be made reliably bounded.
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
        let mut exits = ChildExits::subscribe()?;
        let deadline = Instant::now() + graceful_period;
        #[cfg(test)]
        self.emit(LifecycleEvent::GraceArmed {
            pid: self.pid,
            deadline,
        });
        if self.exited_by(&mut exits, deadline).await? {
            return Ok(());
        }

        let kill_error = self.child.kill().err();
        let reap_deadline = Instant::now() + post_kill_wait;
        #[cfg(test)]
        self.emit(LifecycleEvent::KillIssued {
            pid: self.pid,
            deadline: reap_deadline,
        });
        if self.exited_by(&mut exits, reap_deadline).await? {
            return Ok(());
        }
        let kill_context = kill_error.map_or_else(String::new, |error| {
            format!("; kill request failed: {error}")
        });
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
            let _ = self.child.kill();
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
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

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
            .reap_after_graceful_close(Duration::from_millis(50), Duration::from_secs(30))
            .await
            .expect("kill reaps the child");
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "the kill request waits for the grace period to elapse"
        );
        let observed = drain(&mut events);
        assert_eq!(observed.len(), 3, "{observed:?}");
        let LifecycleEvent::GraceArmed {
            deadline: grace, ..
        } = observed[0]
        else {
            panic!("{observed:?}");
        };
        let LifecycleEvent::KillIssued {
            pid: killed,
            deadline: cleanup,
        } = observed[1]
        else {
            panic!("{observed:?}");
        };
        assert_eq!(killed, pid);
        assert!(
            cleanup >= grace + Duration::from_secs(30),
            "cleanup deadline is armed after the grace deadline passes"
        );
        assert_eq!(observed[2], LifecycleEvent::Reaped { pid });
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
        assert!(started.elapsed() >= Duration::from_millis(50));
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
                    LifecycleEvent::KillIssued { .. },
                    LifecycleEvent::Abandoned { pid: abandoned }
                ] if *abandoned == pid
            ),
            "{observed:?}"
        );
        // The guard's Drop killed the child; like the pool-level abandonment
        // scenarios, the unreaped zombie is the documented residue.
    }
}
