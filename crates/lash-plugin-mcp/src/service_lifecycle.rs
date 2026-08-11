use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::ServiceError;
use rmcp::service::{Peer, RoleClient, RunningService, RunningServiceCancellationToken};

use crate::host::{LashMcpClientHandler, McpHostRequestTasks};

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
