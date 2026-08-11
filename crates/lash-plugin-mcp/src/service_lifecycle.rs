use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use http::{HeaderName, HeaderValue};
use rmcp::ServiceError;
use rmcp::service::{
    Peer, RoleClient, RunningService, RunningServiceCancellationToken, RxJsonRpcMessage,
    ServiceExt, TxJsonRpcMessage,
};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::host::{
    LashMcpClientHandler, McpHostRequestTasks, McpHostServices, McpToolListChangedHandler,
};

struct ManagedChildTransport {
    io: AsyncRwTransport<RoleClient, tokio::fs::File, tokio::fs::File>,
    child: Arc<StdioChildGuard>,
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
        let close_io = self.io.close();
        let child = Arc::clone(&self.child);
        async move {
            close_io.await?;
            child.reap_after_graceful_close().await
        }
    }
}

pub(crate) struct ConnectedService {
    pub(crate) running: RunningService<RoleClient, LashMcpClientHandler>,
    pub(crate) stdio_child: Option<Arc<StdioChildGuard>>,
}

pub(crate) async fn connect_service(
    server_name: &str,
    config: &McpServerConfig,
    host_services: McpHostServices,
    tool_list_changed: Arc<dyn McpToolListChangedHandler>,
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
            let stdio_child = Arc::new(StdioChildGuard::new(child));
            let transport = ManagedChildTransport {
                io: AsyncRwTransport::new(
                    tokio::fs::File::from_std(child_stdout_file(stdout)),
                    tokio::fs::File::from_std(child_stdin_file(stdin)),
                ),
                child: Arc::clone(&stdio_child),
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

pub(crate) struct McpService {
    pub(crate) peer: Peer<RoleClient>,
    pub(crate) request_tasks: Arc<McpHostRequestTasks>,
    pub(crate) cancellation: Option<RunningServiceCancellationToken>,
    pub(crate) quit: Arc<ServiceQuit>,
    pub(crate) stdio_child: Option<Arc<StdioChildGuard>>,
}

impl McpService {
    pub(crate) fn peer(&self) -> &Peer<RoleClient> {
        &self.peer
    }
}

impl Drop for McpService {
    fn drop(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
        if let Some(child) = self.stdio_child.take() {
            child.reap_after_ungraceful_drop();
        }
    }
}

/// Exact child-process handle retained outside rmcp's async service task.
///
/// Ordinary pool drop can move this owned handle to a plain OS thread and
/// synchronously kill-and-wait without relying on the Tokio runtime that may be
/// torn down immediately afterward.
pub(crate) struct StdioChildGuard {
    pid: u32,
    armed: AtomicBool,
    terminate: Arc<AtomicBool>,
    child: std::sync::Mutex<Option<std::process::Child>>,
}

impl StdioChildGuard {
    pub(crate) fn new(child: std::process::Child) -> Self {
        Self {
            pid: child.id(),
            armed: AtomicBool::new(true),
            terminate: Arc::new(AtomicBool::new(false)),
            child: std::sync::Mutex::new(Some(child)),
        }
    }

    async fn reap_after_graceful_close(&self) -> std::io::Result<()> {
        let Some(child) = self.take_child() else {
            return Ok(());
        };
        spawn_child_reaper(self.pid, child, false, Arc::clone(&self.terminate))
            .await
            .map_err(|_| std::io::Error::other("MCP stdio child reaper exited without a result"))?
    }

    pub(crate) fn reap_after_ungraceful_drop(&self) {
        self.terminate.store(true, Ordering::SeqCst);
        let Some(child) = self.take_child() else {
            return;
        };
        drop(spawn_child_reaper(
            self.pid,
            child,
            true,
            Arc::clone(&self.terminate),
        ));
    }

    fn take_child(&self) -> Option<std::process::Child> {
        use lash_sansio::sync::MutexExt;

        if !self.armed.swap(false, Ordering::SeqCst) {
            return None;
        }
        self.child.lock_recover().take()
    }
}

impl Drop for StdioChildGuard {
    fn drop(&mut self) {
        use lash_sansio::sync::LockResultExt;

        self.terminate.store(true, Ordering::SeqCst);
        if self.armed.swap(false, Ordering::SeqCst)
            && let Some(child) = self.child.get_mut().recover().take()
        {
            drop(spawn_child_reaper(
                self.pid,
                child,
                true,
                Arc::clone(&self.terminate),
            ));
        }
    }
}

type ChildReaperTask = Box<dyn FnOnce() + Send + 'static>;

struct ChildReapJob {
    pid: u32,
    child: std::sync::Mutex<Option<std::process::Child>>,
    terminate: Arc<AtomicBool>,
    completion: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<std::io::Result<()>>>>,
}

impl ChildReapJob {
    fn run(&self, kill_immediately: bool) {
        use lash_sansio::sync::MutexExt;

        let Some(child) = self.child.lock_recover().take() else {
            return;
        };
        let result = reap_child(child, kill_immediately, &self.terminate);
        if let Err(error) = &result {
            tracing::warn!(pid = self.pid, %error, "failed to reap MCP stdio child");
        }
        if let Some(completion) = self.completion.lock_recover().take() {
            let _ = completion.send(result);
        }
    }
}

fn spawn_child_reaper(
    pid: u32,
    child: std::process::Child,
    kill_immediately: bool,
    terminate: Arc<AtomicBool>,
) -> tokio::sync::oneshot::Receiver<std::io::Result<()>> {
    spawn_child_reaper_with(pid, child, kill_immediately, terminate, |name, task| {
        std::thread::Builder::new()
            .name(name)
            .spawn(task)
            .map(|_| ())
    })
}

fn spawn_child_reaper_with(
    pid: u32,
    child: std::process::Child,
    kill_immediately: bool,
    terminate: Arc<AtomicBool>,
    spawn: impl FnOnce(String, ChildReaperTask) -> std::io::Result<()>,
) -> tokio::sync::oneshot::Receiver<std::io::Result<()>> {
    let (completion, completed) = tokio::sync::oneshot::channel();
    let job = Arc::new(ChildReapJob {
        pid,
        child: std::sync::Mutex::new(Some(child)),
        terminate,
        completion: std::sync::Mutex::new(Some(completion)),
    });
    let thread_job = Arc::clone(&job);
    if let Err(error) = spawn(
        format!("lash-mcp-reap-{pid}"),
        Box::new(move || thread_job.run(kill_immediately)),
    ) {
        tracing::error!(
            pid,
            %error,
            "failed to spawn MCP stdio child reaper; terminating child synchronously"
        );
        job.run(true);
    }
    completed
}

fn reap_child(
    mut child: std::process::Child,
    kill_immediately: bool,
    terminate: &AtomicBool,
) -> std::io::Result<()> {
    if kill_immediately || terminate.load(Ordering::SeqCst) {
        let _ = child.kill();
        return child.wait().map(|_| ());
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if terminate.load(Ordering::SeqCst) {
            let _ = child.kill();
            return child.wait().map(|_| ());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return child.wait().map(|_| ());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Default)]
pub(crate) struct ServiceQuit {
    finished: AtomicBool,
    notify: tokio::sync::Notify,
}

impl ServiceQuit {
    pub(crate) fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) async fn wait(&self) {
        loop {
            let finished = self.notify.notified();
            if self.finished.load(Ordering::SeqCst) {
                return;
            }
            finished.await;
        }
    }
}

pub(crate) async fn stop_service(mut service: McpService) {
    service.request_tasks.shutdown().await;
    if let Some(cancellation) = service.cancellation.take() {
        cancellation.cancel();
    }
    service.quit.wait().await;
    if let Some(child) = service.stdio_child.take() {
        child.reap_after_ungraceful_drop();
    }
}

pub(crate) async fn cancel_running_service(
    service: RunningService<RoleClient, LashMcpClientHandler>,
) {
    let request_tasks = service.service().request_tasks();
    request_tasks.shutdown().await;
    // `cancel` consumes the service and waits for rmcp's graceful cancellation
    // plus transport-task drain. Errors only surface if the transport already
    // shut down; ignore them.
    let _ = service.cancel().await;
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn reaper_spawn_failure_still_reaps_child() {
        let child = std::process::Command::new("python3")
            .args(["-c", "import time; time.sleep(0.5)"])
            .spawn()
            .expect("spawn test child");
        let pid = child.id();

        let completed = spawn_child_reaper_with(
            pid,
            child,
            true,
            Arc::new(AtomicBool::new(false)),
            |_name, _task| Err(std::io::Error::other("forced reaper spawn failure")),
        );
        completed
            .blocking_recv()
            .expect("fallback reaper must report completion")
            .expect("fallback reaper must reap the child");

        let process_path = format!("/proc/{pid}");
        let deadline = Instant::now() + Duration::from_millis(200);
        while std::path::Path::new(&process_path).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !std::path::Path::new(&process_path).exists(),
            "stdio child PID {pid} survived a forced reaper thread spawn failure"
        );
    }
}
