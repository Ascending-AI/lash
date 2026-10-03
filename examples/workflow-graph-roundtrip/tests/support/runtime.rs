#![allow(
    dead_code,
    reason = "each contract test uses part of the shared fixture"
)]
#![expect(
    clippy::expect_used,
    reason = "the SQLite and Restate-double fixture must initialize"
)]
use lash::restate::restate_sdk;
use workflow_graph_roundtrip::AppState;

pub async fn state() -> (AppState, lash_restate_test::RestateTestBackend) {
    let (state, double, _) = state_and_core().await;
    (state, double)
}

pub async fn state_and_core() -> (
    AppState,
    lash_restate_test::RestateTestBackend,
    lash::LashCore,
) {
    let double = lash_restate_test::backend(4698, Default::default())
        .await
        .expect("Restate double");
    let core =
        workflow_graph_roundtrip::workflow_core(double.lash_backend()).expect("workflow core");
    let worker = lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config().expect("worker config"),
    )
    .expect("worker");
    double.install_process_worker(worker);
    let endpoint = workflow_graph_roundtrip::bind_commands(
        restate_sdk::endpoint::Endpoint::builder(),
        core.clone(),
        double.restate(),
    )
    .build();
    double
        .server()
        .register(endpoint)
        .await
        .expect("host command endpoint");
    (
        AppState::new(core.clone(), double.connection()).expect("workflow state"),
        double,
        core,
    )
}

pub async fn run_workflow(
    client: &reqwest::Client,
    base: &str,
) -> Vec<workflow_graph_roundtrip::RunEvent> {
    let mut response = client
        .post(format!("{base}/run"))
        .send()
        .await
        .expect("POST /run");
    if response.status() != reqwest::StatusCode::OK {
        panic!(
            "POST /run failed: {}",
            response.text().await.expect("error response")
        );
    }
    let mut pending = String::new();
    let mut events = Vec::new();
    let mut signalled = std::collections::BTreeSet::new();
    while let Some(chunk) = response.chunk().await.expect("SSE chunk") {
        pending.push_str(std::str::from_utf8(&chunk).expect("UTF8 events"));
        while let Some(end) = pending.find("\n\n") {
            let frame = pending.drain(..end + 2).collect::<String>();
            if frame.contains("event: run_error") {
                panic!("{frame}");
            }
            for data in frame.lines().filter_map(|line| line.strip_prefix("data: ")) {
                let event: workflow_graph_roundtrip::RunEvent =
                    serde_json::from_str(data).expect("run event");
                if let Some(signal) = &event.waiting_signal
                    && signalled.insert(signal.clone())
                {
                    let response = client
                        .post(format!("{base}/runs/{}/signals/{signal}", event.run_id))
                        .json(&serde_json::json!({"approved": true}))
                        .send()
                        .await
                        .expect("operator signal");
                    assert_eq!(response.status(), reqwest::StatusCode::OK);
                }
                events.push(event);
            }
        }
    }
    events
}
