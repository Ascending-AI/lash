//! Process and session lifecycles release their native resources (FIG-4640).
//! This binary contains one law so its census cannot observe other tests.

#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::testing::ThreadCensus;
use lash_restate_test::{RestateTestBackend, ServerConfig};
use serde_json::json;

fn build_core(backend: &RestateTestBackend) -> lash::LashCore {
    let provider = lash_core::testing::TestProvider::builder()
        .kind("resource-capacity")
        .complete(|_request| async move {
            Ok::<_, lash_core::llm::transport::LlmTransportError>(Default::default())
        })
        .build()
        .into_handle();
    lash::LashCore::standard_builder(backend.lash_backend())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(
            provider,
            lash_core::testing::test_llm_profile_metadata("mock-model"),
        )
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "resource-capacity",
        ))
        .expect("build lifecycle core")
}

async fn one_lifecycle(seed: u64) -> std::sync::Weak<lash_sqlite_store::SqliteStoreSet> {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build lifecycle backend");
    let core = build_core(&backend);
    backend.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("process worker config"),
        )
        .expect("process worker"),
    );
    let env_ref = lash_core::testing::process_execution_env_fixture(
        backend.lash_backend().process_env_store().as_ref(),
    )
    .await;
    let request = lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Engine {
            kind: "testing-fixture".into(),
            payload: json!({}),
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_ref(env_ref);
    let started = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let core = core.clone();
        let started = Arc::clone(&started);
        Arc::new(move |scoped| {
            let core = core.clone();
            let request = request.clone();
            let started = Arc::clone(&started);
            Box::pin(async move {
                let receipt = core
                    .processes()
                    .start(request, scoped)
                    .await
                    .expect("start process");
                *started.lock().unwrap() = Some(receipt.process_id);
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(20),
        backend.run_in_handler(
            lash_core::AdmittedScope::runtime_operation("start"),
            attempt,
        ),
    )
    .await
    .expect("start completes")
    .expect("start handler succeeds");
    let process_id = started.lock().unwrap().take().expect("started process id");
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        core.processes().await_output(&process_id),
    )
    .await
    .expect("process completes")
    .expect("process output");
    assert!(matches!(output,
        lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(&output.outcome, lash_core::ToolCallOutcome::Success(value)
                if value.to_json_value() == json!({"fixture": "complete"}))
    ));
    let session_id = format!("capacity-{seed}");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("create session");
    let session = core.session(session_id).open().await.expect("open session");
    session.close().await.expect("close session");
    backend.server().settle().await;
    for invocation in backend.server().invocations() {
        assert_eq!(invocation.status, "completed", "{invocation:#?}");
    }
    Arc::downgrade(backend.stores())
}

fn lifecycle_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("lifecycle runtime")
}

#[test]
fn process_and_session_lifecycles_release_their_native_resources() {
    // Tokio's first runtime initializes a process-wide signal socket pair.
    // Warm that reusable infrastructure without running a Lash lifecycle.
    drop(lifecycle_runtime());
    let baseline = ThreadCensus::capture().expect("baseline census");
    let runtime = lifecycle_runtime();
    let watches = runtime.block_on(async {
        let mut watches = Vec::new();
        for seed in 0..64 {
            watches.push(one_lifecycle(seed).await);
        }
        watches
    });
    drop(runtime);
    let released = ThreadCensus::capture().expect("released census");
    // Zero slack: ownership teardown must finish before this single census.
    assert_eq!(
        released, baseline,
        "native resources survived ownership teardown"
    );
    for (run, stores) in watches.iter().enumerate() {
        assert_eq!(
            stores.strong_count(),
            0,
            "lifecycle {run} retained its store set"
        );
    }
    println!(
        "capacity law: 64 processes, 64 sessions; baseline={baseline:?}; released={released:?}"
    );
}
