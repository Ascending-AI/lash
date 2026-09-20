//! Host-owned Restate listener lifecycle.

use super::*;

#[cfg(test)]
pub(crate) fn spawn_restate_endpoint(
    addr: SocketAddr,
    state: AppState,
    process_deployment: lash_restate::RestateProcessDeployment,
    process_worker: lash::durability::DurableProcessWorker,
) {
    tokio::spawn(async move {
        let endpoint = endpoint(state, process_deployment, process_worker).await;
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
    tokio::spawn(async move {
        let endpoint = endpoint(state, process_deployment, process_worker).await;
        restate_sdk::http_server::HttpServer::new(endpoint)
            .serve_with_cancel(listener, async move {
                while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
            })
            .await;
    })
}

async fn endpoint(
    state: AppState,
    process_deployment: lash_restate::RestateProcessDeployment,
    process_worker: lash::durability::DurableProcessWorker,
) -> Endpoint {
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
        .bind(LashProcessAttachImpl.serve())
        .build();
    // Wiring-time check: the deployment knows the lash service surface it
    // requires, and the built endpoint reports what it bound. A missing bind
    // fails startup here rather than the first call that would 404.
    if let Err(error) = process_deployment.assert_endpoint_bound(&endpoint).await {
        panic!("workbench Restate endpoint must bind the lash process service surface: {error}");
    }
    endpoint
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Red-proof for the wiring-time check: an endpoint that forgot
    /// `LashProcessAttach` (and the process workflow itself) must fail
    /// binding validation, naming the missing services.
    #[tokio::test]
    async fn endpoint_missing_lash_bindings_fails_validation() {
        let endpoint = Endpoint::builder()
            .bind(LashDurableWaitWorkflowImpl.serve())
            .bind(LashDurableWaitIndexImpl.serve())
            .build();
        let error = lash_restate::assert_services_bound(
            &endpoint,
            lash_restate::RestateProcessDeployment::required_service_names().as_slice(),
        )
        .await
        .expect_err("an endpoint missing lash process services must not validate");
        let lash_restate::RestateBindingCheckError::UnboundServices(missing) = error else {
            panic!("a bound endpoint's discovery answer yields UnboundServices, not {error}");
        };
        assert_eq!(
            missing,
            ["LashProcessAttach", "LashProcessWorkflow"]
                .map(str::to_string)
                .to_vec(),
        );
    }
}
