use super::*;

pub(crate) struct LiveRestateEndpoint {
    pub(crate) addr: SocketAddr,
    pub(crate) endpoint_url: String,
    pub(crate) deployment_id: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LiveRestateEndpoint {
    pub(crate) async fn start(
        admin_url: &str,
        state: AppState,
        process_deployment: lash_restate::RestateProcessDeployment,
        process_worker: lash::durability::DurableProcessWorker,
    ) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind immutable workbench Restate endpoint");
        Self::start_on_listener(
            admin_url,
            listener,
            state,
            process_deployment,
            process_worker,
            true,
        )
        .await
    }

    pub(crate) async fn restart(
        addr: SocketAddr,
        state: AppState,
        process_deployment: lash_restate::RestateProcessDeployment,
        process_worker: lash::durability::DurableProcessWorker,
        deployment_id: String,
    ) -> Self {
        let listener = std::net::TcpListener::bind(addr)
            .unwrap_or_else(|error| panic!("rebind immutable Restate endpoint {addr}: {error}"));
        Self::start_on_listener(
            "",
            listener,
            state,
            process_deployment,
            process_worker,
            false,
        )
        .await
        .with_deployment_id(deployment_id)
    }

    pub(crate) async fn start_replacing_for_mutation(
        admin_url: &str,
        addr: SocketAddr,
        state: AppState,
        process_deployment: lash_restate::RestateProcessDeployment,
        process_worker: lash::durability::DurableProcessWorker,
    ) -> Self {
        let listener = std::net::TcpListener::bind(addr)
            .unwrap_or_else(|error| panic!("bind mutable Restate endpoint {addr}: {error}"));
        let mut endpoint = Self::start_on_listener(
            admin_url,
            listener,
            state,
            process_deployment,
            process_worker,
            false,
        )
        .await;
        endpoint.deployment_id =
            register_restate_deployment_request(admin_url, &endpoint.endpoint_url, true, true)
                .await;
        endpoint
    }

    async fn start_on_listener(
        admin_url: &str,
        listener: std::net::TcpListener,
        state: AppState,
        process_deployment: lash_restate::RestateProcessDeployment,
        process_worker: lash::durability::DurableProcessWorker,
        register: bool,
    ) -> Self {
        let addr = listener
            .local_addr()
            .expect("read immutable workbench Restate endpoint address");
        let endpoint_url = format!("http://{addr}");
        listener
            .set_nonblocking(true)
            .expect("configure immutable workbench Restate endpoint listener");
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let thread = std::thread::Builder::new()
            .name(format!("restate-endpoint-{addr}"))
            .stack_size(STACK_BUDGET_BYTES)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_stack_size(STACK_BUDGET_BYTES)
                    .enable_all()
                    .build()
                    .expect("build owned Restate endpoint runtime");
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener)
                        .expect("adopt immutable workbench Restate endpoint listener");
                    let (endpoint_shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
                    let task = restate::spawn_owned_restate_endpoint(
                        listener,
                        state,
                        process_deployment,
                        process_worker,
                        shutdown_rx,
                    );
                    ready_tx
                        .send(())
                        .expect("announce owned Restate endpoint readiness");
                    let _ = stop.await;
                    let _ = endpoint_shutdown.send(true);
                    tokio::time::timeout(Duration::from_secs(15), task)
                        .await
                        .expect("owned Restate endpoint task shutdown timeout")
                        .expect("owned Restate endpoint task");
                });
                // Dropping this fixture-owned runtime cancels every accepted
                // handler spawned by the endpoint, making an interrupted turn
                // observable to Restate while preserving the endpoint's storage.
                drop(runtime);
            })
            .expect("spawn owned Restate endpoint runtime thread");
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("owned Restate endpoint runtime did not become ready");
        let deployment_id = if register {
            register_restate_deployment(admin_url, &endpoint_url).await
        } else {
            String::new()
        };
        Self {
            addr,
            endpoint_url,
            deployment_id,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }

    fn with_deployment_id(mut self, deployment_id: String) -> Self {
        self.deployment_id = deployment_id;
        self
    }

    pub(crate) async fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            tokio::time::timeout(
                Duration::from_secs(20),
                tokio::task::spawn_blocking(move || thread.join()),
            )
            .await
            .unwrap_or_else(|_| panic!("Restate endpoint {} did not stop", self.endpoint_url))
            .expect("join owned Restate endpoint runtime thread")
            .expect("owned Restate endpoint runtime thread");
        }
        assert!(
            tokio::net::TcpStream::connect(self.addr).await.is_err(),
            "owned Restate endpoint listener {} remained open after shutdown",
            self.endpoint_url
        );
    }

    pub(crate) async fn stop_after_producers_closed_and_drained(
        &mut self,
        state: &AppState,
        timeout: Duration,
    ) {
        let admin =
            lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::with_client(
                state.restate_admin_url.clone(),
                state.restate_http.clone(),
            ));
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // The caller has stopped every fixture-owned producer before entering
            // this method. Count both work already pinned to this deployment and
            // work not pinned yet: an unpinned admission may still select this
            // endpoint, so ignoring it would make teardown race registration.
            let deployment_open = admin
                .open_invocations_by_deployment()
                .await
                .expect("query open Restate invocations by deployment")
                .into_iter()
                .filter(|row| {
                    row.pinned_deployment_id.is_none()
                        || row.pinned_deployment_id.as_deref() == Some(&self.deployment_id)
                })
                .map(|row| row.open_count)
                .sum::<u64>();
            let lash_drain = state
                .core
                .drain_status(false)
                .await
                .expect("query Lash deployment drain status");
            if deployment_open == 0 && lash_drain.drained {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "immutable Restate endpoint {} ({}) did not drain within {timeout:?}; Restate open={deployment_open}, Lash remaining={}",
                self.endpoint_url,
                self.deployment_id,
                lash_drain.remaining_invocations,
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.stop().await;
    }
}

impl Drop for LiveRestateEndpoint {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        // A panic may prevent the async stop path from joining. The shutdown
        // signal still makes the owned thread and runtime retire promptly.
        let _ = self.thread.take();
    }
}

#[derive(Deserialize)]
struct RestateDeploymentRegistration {
    id: String,
}

pub(crate) async fn register_restate_deployment(admin_url: &str, endpoint_url: &str) -> String {
    register_restate_deployment_request(admin_url, endpoint_url, false, false).await
}

async fn register_restate_deployment_request(
    admin_url: &str,
    endpoint_url: &str,
    force: bool,
    breaking: bool,
) -> String {
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("build Restate admin client");
    let response = client
        .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
        .json(&json!({
            "uri": endpoint_url,
            "force": force,
            "breaking": breaking,
        }))
        .send()
        .await
        .expect("register deployment with Restate admin API");
    let status = response.status();
    let body = response
        .bytes()
        .await
        .expect("read Restate deployment registration response");
    assert!(
        status.is_success(),
        "Restate deployment registration failed: {status} {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice::<RestateDeploymentRegistration>(&body)
        .expect("decode Restate deployment registration response")
        .id
}

pub(crate) async fn restate_invocation_status_with_deployment(
    admin_url: &str,
    invocation_id: &lash_restate::RestateInvocationId,
) -> Option<lash_restate::RestateInvocationStatus> {
    let escaped_id = invocation_id.as_str().replace('\'', "''");
    let mut rows =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::new(admin_url))
            .query_json::<lash_restate::RestateInvocationStatus>(&format!(
                "SELECT id, target, target_service_name, target_service_key, \
         target_handler_name, status, completion_result, completion_failure, \
         pinned_deployment_id FROM sys_invocation WHERE id = '{escaped_id}'"
            ))
            .await
            .expect("query deployment-aware Restate invocation status");
    rows.pop()
}
