//! Host-owned Restate listener lifecycle.

use super::*;

#[cfg(test)]
pub(crate) fn spawn_restate_endpoint(
    addr: SocketAddr,
    state: AppState,
    process_deployment: lash_restate::RestateProcessDeployment,
    process_worker: lash::durability::DurableProcessWorker,
) {
    let endpoint = endpoint(state, process_deployment, process_worker);
    tokio::spawn(async move {
        restate_sdk::http_server::HttpServer::new(endpoint)
            .listen_and_serve(addr)
            .await;
    });
}

/// Start on a listener the host already owns and retain the join authority.
///
/// Restate SDK 0.11 stops listener intake and applies its fixed ten-second
/// connection grace before this task returns. The SDK does not expose its
/// accepted-connection task set, so awaiting this handle does not establish
/// that every active handler completed; the host deliberately adds no new
/// durable-turn cancellation policy here.
pub(crate) fn spawn_owned_restate_endpoint(
    listener: tokio::net::TcpListener,
    state: AppState,
    process_deployment: lash_restate::RestateProcessDeployment,
    process_worker: lash::durability::DurableProcessWorker,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let endpoint = endpoint(state, process_deployment, process_worker);
    tokio::spawn(async move {
        restate_sdk::http_server::HttpServer::new(endpoint)
            .serve_with_cancel(listener, async move {
                while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
            })
            .await;
    })
}

fn endpoint(
    state: AppState,
    process_deployment: lash_restate::RestateProcessDeployment,
    process_worker: lash::durability::DurableProcessWorker,
) -> Endpoint {
    Endpoint::builder()
        .bind(WorkbenchTurnWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchQueuedTurnWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchButtonTriggerWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchMailReceivedWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchSessionDeleteWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchProcessCancelWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchCronJobImpl::new(state).serve())
        .bind(process_deployment.workflow(process_worker).serve())
        .bind(LashDurableWaitWorkflowImpl.serve())
        .bind(LashDurableWaitIndexImpl.serve())
        .build()
}
