//! RLM frame evidence for ADR 0113. The Restate double runs the real engine.

#![cfg(all(
    feature = "rlm",
    feature = "restate",
    feature = "sqlite",
    feature = "testing"
))]
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;
use lash::{LashCore, TurnInput, TurnOutput};
use lash_sansio::sync::MutexExt;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    artifact_ref: String,
    kind: String,
    id: String,
}

#[expect(
    clippy::expect_used,
    reason = "acceptance fixture reads the durable edge table"
)]
fn sqlite_edges(double: &lash_restate_test::RestateTestBackend) -> Vec<Edge> {
    let uri = double
        .stores()
        .database_uri(lash_sqlite_store::SqliteDatabase::DurableCore);
    let connection = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open durable core for edge inspection");
    let mut statement = connection
        .prepare(
            "SELECT artifact_ref, referrer_kind, referrer_id
         FROM artifact_referrer_edges WHERE namespace = 'lashlang_module'
         ORDER BY artifact_ref, referrer_kind, referrer_id",
        )
        .expect("prepare edge read");
    statement
        .query_map([], |row| {
            Ok(Edge {
                artifact_ref: row.get(0)?,
                kind: row.get(1)?,
                id: row.get(2)?,
            })
        })
        .expect("read edges")
        .map(|row| row.expect("decode edge"))
        .collect()
}

async fn wait_edges(
    double: &lash_restate_test::RestateTestBackend,
    predicate: impl Fn(&[Edge]) -> bool,
) -> Vec<Edge> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let edges = sqlite_edges(double);
            if predicate(&edges) {
                return edges;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "artifact edges did not reach the expected state: {:?}",
            sqlite_edges(double)
        )
    })
}

fn frame_artifacts(edges: &[Edge]) -> BTreeSet<&str> {
    edges
        .iter()
        .filter(|edge| edge.kind == "frame_environment")
        .map(|edge| edge.artifact_ref.as_str())
        .collect()
}

fn last_cell_finish(output: &TurnOutput) -> Option<serde_json::Value> {
    output
        .result
        .state
        .session_graph
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            lash_core::SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(event),
            } if event.plugin_id == "rlm_protocol" => event
                .payload
                .get("RlmTrajectoryEntry")?
                .get("final_output")
                .cloned(),
            _ => None,
        })
        .last()
}

fn response(code: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: format!("<typescript>\n{code}\n</typescript>"),
            response_meta: None,
        }],
        ..Default::default()
    }
}

#[expect(clippy::expect_used, reason = "test fixture validates its setup")]
fn rlm_core(
    double: &lash_restate_test::RestateTestBackend,
    responses: Vec<LlmResponse>,
) -> LashCore {
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    let provider = lash::testing::TestProvider::builder()
        .kind("artifact-referrers")
        .complete(move |_request| {
            let queue = Arc::clone(&queue);
            async move {
                Ok(queue
                    .lock_recover()
                    .pop_front()
                    .expect("scripted response queue is exhausted"))
            }
        })
        .build()
        .into_handle();
    let backend = double.lash_backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("artifact-referrers")
                .context_window_tokens(16_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "artifact-referrers-worker",
            "artifact-referrers-boot",
        ))
        .expect("RLM core")
}

#[expect(
    clippy::expect_used,
    reason = "test fixture installs its process worker"
)]
fn serve_processes(double: &lash_restate_test::RestateTestBackend, core: &LashCore) {
    let worker = lash_core_worker::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .expect("process-worker config"),
    )
    .expect("process worker");
    double.install_process_worker(worker);
}

#[expect(clippy::expect_used, reason = "acceptance test validates each turn")]
#[tokio::test]
async fn cold_reopen_globals_across_turns() {
    let double =
        lash_restate_test::backend(0x4031_0001, lash_restate_test::ServerConfig::default())
            .await
            .expect("Restate double");
    let first_core = rlm_core(
        &double,
        vec![response("const saved = async () => 7; finish('bound');")],
    );
    let first_session = first_core
        .session("artifact-referrers-cold-reopen")
        .open()
        .await
        .expect("open first session");
    let first = first_session
        .send(TurnInput::text("bind definition"))
        .output()
        .await
        .expect("first turn");
    assert!(first.is_success());
    let first_edges = wait_edges(&double, |edges| {
        edges.len() == 2
            && edges.iter().any(|edge| edge.kind == "frame_environment")
            && edges.iter().any(|edge| edge.kind == "execution")
    })
    .await;
    let frame_id = first_edges
        .iter()
        .find(|edge| edge.kind == "frame_environment")
        .expect("frame edge")
        .id
        .clone();
    drop(first_session);
    drop(first_core);

    let second_core = rlm_core(
        &double,
        vec![response(
            "const run = await processes.start({ definition: saved }); finish(await run);",
        )],
    );
    serve_processes(&double, &second_core);
    let second_session = second_core
        .session("artifact-referrers-cold-reopen")
        .open()
        .await
        .expect("cold reopen");
    let second = second_session
        .send(TurnInput::text("use definition"))
        .output()
        .await
        .expect("second turn");
    assert!(
        second.is_success(),
        "a global from the first turn survives cold reopen: {second:?}"
    );
    assert_eq!(last_cell_finish(&second), Some(serde_json::json!(7)));
    let after_start = wait_edges(&double, |edges| {
        edges
            .iter()
            .any(|edge| edge.kind == "frame_environment" && edge.id == frame_id)
    })
    .await;
    assert_eq!(
        frame_artifacts(&after_start).len(),
        1,
        "both turns use one frame"
    );
}

#[expect(
    clippy::expect_used,
    reason = "acceptance test validates each turn and edge set"
)]
#[tokio::test]
async fn overwrite_retains_old_module_until_frame_end() {
    let double =
        lash_restate_test::backend(0x4031_0002, lash_restate_test::ServerConfig::default())
            .await
            .expect("Restate double");
    let core = rlm_core(
        &double,
        vec![
            response(
                "let holder = new Map(); { const old = async () => 11; holder.set('saved', old); } finish('first');",
            ),
            response(
                "{ const newer = async () => 22; holder.set('saved', newer); } finish('second');",
            ),
            response("await control.continue_as({ task: 'new frame' });"),
            response("finish('new frame');"),
        ],
    );
    let session = core
        .session("artifact-referrers-overwrite")
        .open()
        .await
        .expect("session");
    let first_turn = session
        .send(TurnInput::text("bind first"))
        .output()
        .await
        .expect("first turn");
    assert!(first_turn.is_success(), "first binding: {first_turn:?}");
    let first = wait_edges(&double, |edges| frame_artifacts(edges).len() == 1).await;
    let old_ref = frame_artifacts(&first)
        .into_iter()
        .next()
        .expect("old module")
        .to_owned();
    assert!(
        session
            .send(TurnInput::text("overwrite"))
            .output()
            .await
            .expect("second turn")
            .is_success()
    );
    let rebound = wait_edges(&double, |edges| frame_artifacts(edges).len() == 2).await;
    assert!(
        frame_artifacts(&rebound).contains(old_ref.as_str()),
        "overwrite retains the old frame edge"
    );
    assert!(
        session
            .send(TurnInput::text("switch"))
            .output()
            .await
            .expect("switch turn")
            .is_success()
    );
    let after = wait_edges(&double, |edges| {
        !edges.iter().any(|edge| edge.artifact_ref == old_ref)
    })
    .await;
    assert!(
        !after.iter().any(|edge| edge.artifact_ref == old_ref),
        "old bytes are reclaimed after frame end"
    );
}

#[expect(
    clippy::expect_used,
    reason = "acceptance test validates the carry and the absent edge"
)]
#[tokio::test]
async fn continue_as_carries_only_seeded_definition() {
    let double =
        lash_restate_test::backend(0x4031_0003, lash_restate_test::ServerConfig::default())
            .await
            .expect("Restate double");
    let core = rlm_core(
        &double,
        vec![
            response("const carried = async () => 11; finish('carried bound');"),
            response("const dropped = async () => 22; finish('dropped bound');"),
            response("await control.continue_as({ task: 'use seed', seed: { carried } });"),
            response(
                "const run = await processes.start({ definition: carried }); finish(await run);",
            ),
            response(
                "const run = await processes.start({ definition: carried }); finish(await run);",
            ),
        ],
    );
    serve_processes(&double, &core);
    let session = core
        .session("artifact-referrers-carry")
        .open()
        .await
        .expect("session");
    let first_turn = session
        .send(TurnInput::text("bind carried definition"))
        .output()
        .await
        .expect("first turn");
    assert!(first_turn.is_success(), "first binding: {first_turn:?}");
    let carried_before = wait_edges(&double, |edges| frame_artifacts(edges).len() == 1).await;
    let carried_ref = frame_artifacts(&carried_before)
        .into_iter()
        .next()
        .expect("carried module")
        .to_owned();
    assert!(
        session
            .send(TurnInput::text("bind dropped definition"))
            .output()
            .await
            .expect("second turn")
            .is_success()
    );
    let before = wait_edges(&double, |edges| frame_artifacts(edges).len() == 2).await;
    let dropped_ref = frame_artifacts(&before)
        .into_iter()
        .find(|artifact_ref| *artifact_ref != carried_ref)
        .expect("second definition has its own module")
        .to_owned();
    let old_frame = before
        .iter()
        .find(|edge| edge.kind == "frame_environment")
        .expect("frame edge")
        .id
        .clone();
    let switched = session
        .send(TurnInput::text("carry one"))
        .output()
        .await
        .expect("switch turn");
    assert!(switched.is_success(), "carry switch: {switched:?}");
    let after = wait_edges(&double, |edges| {
        let frame: Vec<_> = edges
            .iter()
            .filter(|edge| edge.kind == "frame_environment")
            .collect();
        frame.len() == 1
            && frame[0].id != old_frame
            && frame[0].artifact_ref == carried_ref
            && !edges.iter().any(|edge| edge.artifact_ref == dropped_ref)
    })
    .await;
    assert_eq!(frame_artifacts(&after).len(), 1);
    assert!(
        !after.iter().any(|edge| edge.id == old_frame),
        "the ended frame holds no edge"
    );
    let result = session
        .send(TurnInput::text("run carried definition"))
        .output()
        .await
        .expect("new-frame turn");
    assert!(result.is_success());
    assert_eq!(last_cell_finish(&result), Some(serde_json::json!(11)));
}

#[expect(
    clippy::expect_used,
    reason = "acceptance test validates the definition revision edge"
)]
#[tokio::test]
async fn named_definition_survives_uncarried_frame_switch() {
    let double =
        lash_restate_test::backend(0x4031_0012, lash_restate_test::ServerConfig::default())
            .await
            .expect("Restate double");
    let core = rlm_core(
        &double,
        vec![
            response(
                "const named = async () => 31; await processes.register({ name: 'saved', definition: named }); finish('registered');",
            ),
            response("await control.continue_as({ task: 'use saved name' });"),
            response("finish('switched');"),
        ],
    );
    let session = core
        .session("artifact-referrers-named")
        .open()
        .await
        .expect("session");
    assert!(
        session
            .send(TurnInput::text("register definition"))
            .output()
            .await
            .expect("registration turn")
            .is_success()
    );
    let registered = wait_edges(&double, |edges| {
        edges.iter().any(|edge| edge.kind == "definition_revision")
    })
    .await;
    let module_ref = registered
        .iter()
        .find(|edge| edge.kind == "definition_revision")
        .expect("revision edge")
        .artifact_ref
        .clone();
    assert!(
        session
            .send(TurnInput::text("switch without seed"))
            .output()
            .await
            .expect("switch turn")
            .is_success()
    );
    let after = wait_edges(&double, |edges| {
        edges
            .iter()
            .any(|edge| edge.artifact_ref == module_ref && edge.kind == "definition_revision")
            && !edges
                .iter()
                .any(|edge| edge.artifact_ref == module_ref && edge.kind == "frame_environment")
    })
    .await;
    assert_eq!(
        after
            .iter()
            .filter(|edge| edge.artifact_ref == module_ref)
            .count(),
        1
    );
    drop(session);
    drop(core);
    let registry = double.lash_backend().process_definition_registry();
    let saved = lash_core::process_registry::resolve_named_definition(
        registry.as_ref(),
        &lash_core::SessionId::from("artifact-referrers-named"),
        "saved",
    )
    .await
    .expect("resolve registered name")
    .expect("the registered name survives the frame switch");
    assert_eq!(
        saved.definition.definition.as_json()["module_ref"],
        module_ref
    );
    assert!(
        double
            .lash_backend()
            .module_artifacts()
            .get_module_artifact(&module_ref)
            .await
            .expect("read pinned module")
            .is_some(),
        "the registered definition's module survives its frame"
    );
}
