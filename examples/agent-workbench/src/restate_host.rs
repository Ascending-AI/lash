//! Host-owned Restate listener lifecycle.

#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

use lash_restate::{
    LashDurableWaitIndex, LashDurableWaitIndexImpl, LashDurableWaitWorkflow,
    LashDurableWaitWorkflowImpl, LashProcessWorkflow,
};
use restate_sdk::prelude::Endpoint;

use crate::AppState;
use crate::restate::{
    WorkbenchButtonTriggerWorkflow, WorkbenchButtonTriggerWorkflowImpl, WorkbenchCronJob,
    WorkbenchCronJobImpl, WorkbenchMailReceivedWorkflow, WorkbenchMailReceivedWorkflowImpl,
    WorkbenchProcessCancelWorkflow, WorkbenchProcessCancelWorkflowImpl,
    WorkbenchQueuedTurnWorkflow, WorkbenchQueuedTurnWorkflowImpl, WorkbenchSessionDeleteWorkflow,
    WorkbenchSessionDeleteWorkflowImpl, WorkbenchTurnWorkflow, WorkbenchTurnWorkflowImpl,
};

/// Start on a listener the host already owns and retain the join authority.
pub(crate) fn spawn_owned_restate_endpoint(
    listener: tokio::net::TcpListener,
    state: AppState,
    process_deployment: lash_restate::RestateProcessDeployment,
    process_worker: lash::durability::DurableProcessWorker,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let endpoint = Endpoint::builder()
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
        .build();
    tokio::spawn(async move {
        restate_sdk::http_server::HttpServer::new(endpoint)
            .serve_with_cancel(listener, async move {
                while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
            })
            .await;
    })
}
