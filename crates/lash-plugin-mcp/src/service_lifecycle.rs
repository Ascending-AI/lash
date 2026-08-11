use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use http::{HeaderName, HeaderValue};
use rmcp::ServiceError;
use rmcp::service::{RoleClient, RunningService, RxJsonRpcMessage, ServiceExt, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::host::{LashMcpClientHandler, McpHostServices, McpToolListChangedHandler};

pub(crate) const STDIO_GRACEFUL_SHUTDOWN_PERIOD: Duration = Duration::from_secs(3);
const STDIO_POST_KILL_WAIT_BOUND: Duration = Duration::from_secs(1);

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

pub(crate) struct ConnectedService {
    pub(crate) running: RunningService<RoleClient, LashMcpClientHandler>,
    pub(crate) stdio_child: Option<StdioChildGuard>,
}

pub(crate) async fn connect_service(
    server_name: &str,
    config: &McpServerConfig,
    host_services: McpHostServices,
    tool_list_changed: Arc<dyn McpToolListChangedHandler>,
    active_pid: Arc<AtomicU32>,
    shutdown_requested: Arc<AtomicBool>,
) -> Result<ConnectedService, McpError> {
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
            let mut child = cmd.spawn().map_err(|err| {
                McpError::Protocol(format!(
                    "failed to spawn `{command}` for `{server_name}`: {err}"
                ))
            })?;
            let stdout = child.stdout.take().ok_or_else(|| {
                McpError::Protocol(format!(
                    "failed to capture stdout for `{command}` MCP server `{server_name}`"
                ))
            })?;
            let stdin = child.stdin.take().ok_or_else(|| {
                McpError::Protocol(format!(
                    "failed to capture stdin for `{command}` MCP server `{server_name}`"
                ))
            })?;
            let stdio_child = StdioChildGuard::new(server_name, child, shutdown_requested);
            active_pid.store(stdio_child.pid(), Ordering::SeqCst);
            let transport = ManagedChildTransport {
                io: AsyncRwTransport::new(
                    tokio::fs::File::from_std(child_stdout_file(stdout)),
                    tokio::fs::File::from_std(child_stdin_file(stdin)),
                ),
            };
            let running = client_handler.serve(transport).await.map_err(|err| {
                McpError::Protocol(format!("MCP handshake with `{server_name}`: {err}"))
            })?;
            Ok(ConnectedService {
                running,
                stdio_child: Some(stdio_child),
            })
        }
        McpServerConfig::StreamableHttp { url, headers, .. } => {
            active_pid.store(0, Ordering::SeqCst);
            let custom_headers = build_http_headers(server_name, headers)?;
            let config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                .custom_headers(custom_headers);
            let transport = StreamableHttpClientTransport::from_config(config);
            let running = client_handler.serve(transport).await.map_err(|err| {
                McpError::Protocol(format!("MCP handshake with `{server_name}`: {err}"))
            })?;
            Ok(ConnectedService {
                running,
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

/// Exact child-process handle retained outside rmcp's async service task.
///
/// Explicit shutdown closes the child's stdin, gives it a grace period, and
/// waits to reap it. Dropping the guard without that shutdown only kills and
/// logs: waiting in `Drop` cannot be made reliably bounded.
pub(crate) struct StdioChildGuard {
    server_name: String,
    pid: u32,
    child: std::process::Child,
    reaped: bool,
    explicit_abandonment: bool,
    shutdown_requested: Arc<AtomicBool>,
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
        }
    }

    pub(crate) async fn reap_after_graceful_close(mut self) -> std::io::Result<()> {
        self.explicit_abandonment = true;
        let deadline = Instant::now() + STDIO_GRACEFUL_SHUTDOWN_PERIOD;
        loop {
            if self.child.try_wait()?.is_some() {
                self.reaped = true;
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let kill_error = self.child.kill().err();
        let reap_deadline = Instant::now() + STDIO_POST_KILL_WAIT_BOUND;
        loop {
            if self.child.try_wait()?.is_some() {
                self.reaped = true;
                return Ok(());
            }
            if Instant::now() >= reap_deadline {
                let kill_context = kill_error.map_or_else(String::new, |error| {
                    format!("; kill request failed: {error}")
                });
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "MCP stdio child PID {} did not exit within 1s after the kill request{kill_context}",
                        self.pid
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn begin_bounded_cleanup(&mut self) {
        self.explicit_abandonment = true;
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
        }
    }
}
