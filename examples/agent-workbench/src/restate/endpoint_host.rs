//! Host-owned Restate listener lifecycle.

use super::*;

#[cfg(test)]
pub(crate) fn spawn_restate_endpoint(
    addr: SocketAddr,
    state: AppState,
    backend: Arc<crate::WorkbenchRestateBackend>,
    process_worker: lash::durability::DurableProcessWorker,
) {
    tokio::spawn(async move {
        let endpoint = endpoint(state, backend, process_worker);
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
    backend: Arc<crate::WorkbenchRestateBackend>,
    process_worker: lash::durability::DurableProcessWorker,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let endpoint = endpoint(state, backend, process_worker);
        restate_sdk::http_server::HttpServer::new(endpoint)
            .serve_with_cancel(listener, async move {
                while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
            })
            .await;
    })
}

/// The workbench's Restate endpoint: lash's own services come from the
/// backend, and the workbench binds only its turn, trigger, session and cron
/// workflows beside them.
fn endpoint(
    state: AppState,
    backend: Arc<crate::WorkbenchRestateBackend>,
    process_worker: lash::durability::DurableProcessWorker,
) -> Endpoint {
    backend
        .endpoint_builder(process_worker)
        .bind(lash_restate::turn_service(
            WorkbenchTurnWorkflowImpl::new(state.clone()).serve(),
            "run",
        ))
        .bind(lash_restate::turn_service(
            WorkbenchQueuedTurnWorkflowImpl::new(state.clone()).serve(),
            "run",
        ))
        .bind(WorkbenchButtonTriggerWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchMailReceivedWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchSessionDeleteWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchProcessCancelWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchCronJobImpl::new(state).serve())
        .build()
}
