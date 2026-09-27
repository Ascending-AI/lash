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
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("bind the workbench Restate endpoint");
        lash::restate::serve_endpoint(listener, endpoint, std::future::pending::<()>()).await;
    });
}

/// Start on a listener the host already owns and retain the join authority.
///
/// [`lash::restate::serve_endpoint`] stops listener intake and applies its
/// fixed ten-second connection grace before this task returns, so awaiting
/// this handle does not establish that every active handler completed; the
/// host deliberately adds no new durable-turn cancellation policy here.
pub(crate) fn spawn_owned_restate_endpoint(
    listener: tokio::net::TcpListener,
    state: AppState,
    backend: Arc<crate::WorkbenchRestateBackend>,
    process_worker: lash::durability::DurableProcessWorker,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let endpoint = endpoint(state, backend, process_worker);
        lash::restate::serve_endpoint(listener, endpoint, async move {
            while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
        })
        .await;
    })
}

/// The workbench's Restate endpoint: lash's own services come from the
/// backend, and `LashSession` among them drives every turn; the workbench
/// binds only its trigger, session and cron workflows beside them.
fn endpoint(
    state: AppState,
    backend: Arc<crate::WorkbenchRestateBackend>,
    process_worker: lash::durability::DurableProcessWorker,
) -> Endpoint {
    backend
        .endpoint_builder(process_worker)
        .bind(WorkbenchButtonTriggerWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchMailReceivedWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchSessionDeleteWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchProcessCancelWorkflowImpl::new(state.clone()).serve())
        .bind(WorkbenchCronJobImpl::new(state).serve())
        .build()
}
