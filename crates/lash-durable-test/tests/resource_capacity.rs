//! Process and session lifecycles release their native resources (FIG-4640),
//! on the durable engine.
//!
//! Each lifecycle builds a deployment the way a host does, a durable backend
//! over a fresh SQLite store set and a lash core over it whose node serves
//! its actors, runs one engine process to its terminal and one session turn
//! through the facade, then shuts the core down and drops it. After them all,
//! the process holds exactly the native threads and file descriptors it held
//! before the first (beside process-wide pools warmed before the baseline),
//! and no lifecycle's store set is still owned.
//!
//! This binary holds one law, so its census observes no other test.

#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use lash_core::testing::ThreadCensus;
use lash_core_execution::StoreSet;
use serde_json::json;

const MODEL: &str = "resource-capacity-model";
/// How many lifecycles the census spans.
const LIFECYCLES: usize = 32;

/// The environment a process runs under: what the core's node renders and
/// admits with.
fn environment() -> lash_core::ProcessExecutionEnvSpec {
    let mut environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(16),
            lash_core::NoProgressBudget::bounded(12),
        ),
    );
    environment.render = Some(lash_core::RecordedRender {
        renderer_id: lash::render::ToolOutputRendererSlot::default()
            .0
            .id()
            .to_owned(),
        params: serde_json::to_value(lash::render::ResolvedStandardRenderConfig {
            defaults: lash::render::ToolRenderParams::default(),
            per_tool: std::collections::BTreeMap::new(),
        })
        .expect("the render config encodes"),
    });
    environment
}

/// One lifecycle: a deployment that runs a process and a turn, shut down
/// and dropped. Answers a watch on its store set.
async fn one_lifecycle(run: usize) -> std::sync::Weak<lash_sqlite_store::SqliteStoreSet> {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let watch = Arc::downgrade(&stores);
    let backend = lash::durable::DurableBackendBuilder::new(stores as Arc<dyn StoreSet>)
        .process_engine(Arc::new(lash_core_execution::testing::FixtureProcessEngine))
        .build()
        .expect("the backend assembles");
    let provider = lash_core::testing::TestProvider::builder()
        .kind("resource-capacity")
        .complete(|_request| async move {
            Ok::<_, lash_core::llm::transport::LlmTransportError>(lash_core::LlmResponse {
                parts: vec![lash_core::LlmOutputPart::Text {
                    text: "done".to_owned(),
                    response_meta: None,
                }],
                ..lash_core::LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend.clone())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .serve_test_llm_profile(
            provider,
            lash_core::LlmProfileMetadata::builder(MODEL)
                .context_window_tokens(200_000)
                .build()
                .expect("the model's metadata"),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "resource-capacity",
            format!("lifecycle-{run}"),
        ))
        .expect("the core builds");

    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &environment())
        .await
        .expect("the environment is published");
    let process = core
        .processes()
        .start(
            lash_core::ProcessStartRequest::new(
                lash_core::ProcessInput::Engine {
                    kind: "testing-fixture".to_owned(),
                    payload: json!({}),
                },
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            )
            .with_env_ref(env_ref),
            core.effect_host(),
        )
        .await
        .expect("the process starts")
        .process_id;
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        core.processes().await_output(&process),
    )
    .await
    .expect("the process ends within a minute")
    .expect("the process's end is read");
    assert!(
        matches!(&output, lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(&output.outcome, lash_core::ToolCallOutcome::Success(value)
                if value.to_json_value() == json!({ "fixture": "complete" }))),
        "{output:?}"
    );

    let session = core
        .session(lash_core::SessionId::fixture(format!("capacity-{run}")))
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(16),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
        .expect("the session is created");
    let answered = tokio::time::timeout(
        Duration::from_secs(60),
        session.send(lash::TurnInput::text("answer")).output(),
    )
    .await
    .expect("the turn answers within a minute")
    .expect("the turn's output");
    assert!(answered.is_success(), "{:?}", answered.result.outcome);
    drop(session);
    core.shutdown().await.expect("the core shuts down");
    drop(core);
    drop(backend);
    watch
}

fn lifecycle_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("the lifecycle runtime")
}

#[test]
fn process_and_session_lifecycles_release_their_native_resources() {
    // Tokio's first runtime initializes a process-wide signal socket pair,
    // and the first prompt composition starts the process's shared render
    // pool: warm that reusable infrastructure without running a lifecycle.
    // Pool construction waits for every worker's startup, so the baseline
    // sees their final native names even if scheduling delayed those threads.
    drop(lifecycle_runtime());
    let _ = lash_core_execution::plugin::prompt::PromptRenderPool::shared();
    let baseline = ThreadCensus::capture().expect("the baseline census");
    let runtime = lifecycle_runtime();
    let watches = runtime.block_on(async {
        let mut watches = Vec::new();
        for run in 0..LIFECYCLES {
            watches.push(one_lifecycle(run).await);
        }
        watches
    });
    drop(runtime);
    let released = ThreadCensus::capture().expect("the released census");
    // No slack: teardown finishes before this one census.
    assert_eq!(
        released, baseline,
        "native resources survived the lifecycles' teardown"
    );
    for (run, stores) in watches.iter().enumerate() {
        assert_eq!(
            stores.strong_count(),
            0,
            "lifecycle {run} kept its store set"
        );
    }
}
