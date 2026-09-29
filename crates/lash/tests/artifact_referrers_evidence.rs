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
        .next_back()
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

fn rlm_core(
    double: &lash_restate_test::RestateTestBackend,
    responses: Vec<LlmResponse>,
) -> LashCore {
    rlm_core_with_queue(double, Arc::new(Mutex::new(VecDeque::from(responses))))
}

#[expect(clippy::expect_used, reason = "test fixture validates its setup")]
fn rlm_core_with_queue(
    double: &lash_restate_test::RestateTestBackend,
    queue: Arc<Mutex<VecDeque<LlmResponse>>>,
) -> LashCore {
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

#[tokio::test]
async fn cold_reopen_globals_across_turns() {
    let double =
        lash_restate_test::backend(0x4031_0001, lash_restate_test::ServerConfig::default())
            .await
            .expect("Restate double");
    // A reopened core may use either provider handle for the same route.
    // Both handles therefore draw from the two-turn script in call order.
    let responses = Arc::new(Mutex::new(VecDeque::from(vec![
        response("const saved = async () => 7; finish('bound');"),
        response("const run = await processes.start({ definition: saved }); finish(await run);"),
    ])));
    let first_core = rlm_core_with_queue(&double, Arc::clone(&responses));
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
    let settled = wait_edges(&double, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment" && edges[0].id == frame_id
    })
    .await;
    assert_eq!(settled.len(), 1, "the settled first turn holds no edge");
    drop(first_session);
    drop(first_core);

    let second_core = rlm_core_with_queue(&double, Arc::clone(&responses));
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
    assert!(
        responses.lock_recover().is_empty(),
        "each of the two turns consumes one scripted response"
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

#[tokio::test]
async fn first_turn_continue_as_fences_its_initial_frame() {
    let double =
        lash_restate_test::backend(0x4031_0004, lash_restate_test::ServerConfig::default())
            .await
            .expect("Restate double");
    let core = rlm_core(
        &double,
        vec![
            response(
                "const old = async () => 5; await control.continue_as({ task: 'next frame' });",
            ),
            response("finish('next frame');"),
        ],
    );
    let session = core
        .session("artifact-referrers-first-switch")
        .open()
        .await
        .expect("session");
    let switched = session
        .send(TurnInput::text("switch on first turn"))
        .output()
        .await
        .expect("switch turn");
    assert!(switched.is_success(), "first-turn switch: {switched:?}");
    let frames = &switched.result.state.agent_frames;
    assert_eq!(frames.len(), 2, "the switch opens a second frame");
    let first_frame = frames[0].frame_node_id.as_str();
    let after = wait_edges(&double, |edges| {
        !edges
            .iter()
            .any(|edge| edge.kind == "frame_environment" && edge.id == first_frame)
    })
    .await;
    assert!(
        !after
            .iter()
            .any(|edge| edge.kind == "frame_environment" && edge.id == first_frame),
        "the first frame has no surviving edge"
    );
}

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
            && edges
                .iter()
                .filter(|edge| edge.artifact_ref == module_ref)
                .count()
                == 1
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

/// The source every `processes.create` case compiles: one process, whose
/// answer a later turn reads back to prove it ran the created module.
const CREATED_SOURCE: &str = "const answer = async () => 40 + 2;";

fn create_definition_cell(binding: &str) -> String {
    format!(
        "const {binding} = await processes.create({{ source: {CREATED_SOURCE:?}, dialect: 'typescript' }}); finish('created');"
    )
}

/// FIG-3116: a definition `processes.create` returns is an RLM value like
/// any other (ADR 0113 §6). The call's realization publishes its module under
/// the realizing execution, the cell's global holds it in the frame, and a
/// later turn in the same frame starts it by value after a cold reopen.
#[tokio::test]
async fn created_definition_survives_cold_reopen_and_starts_by_value() {
    let double =
        lash_restate_test::backend(0x3116_0001, lash_restate_test::ServerConfig::default())
            .await
            .expect("Restate double");
    let responses = Arc::new(Mutex::new(VecDeque::from(vec![
        response(&create_definition_cell("made")),
        response("const run = await processes.start({ definition: made }); finish(await run);"),
    ])));
    let first_core = rlm_core_with_queue(&double, Arc::clone(&responses));
    let first_session = first_core
        .session("processes-create-cold-reopen")
        .open()
        .await
        .expect("open first session");
    let first = first_session
        .send(TurnInput::text("create definition"))
        .output()
        .await
        .expect("first turn");
    assert!(first.is_success(), "processes.create turn: {first:?}");
    let created = wait_edges(&double, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment"
    })
    .await;
    let module_ref = created[0].artifact_ref.clone();
    let frame_id = created[0].id.clone();
    drop(first_session);
    drop(first_core);

    let second_core = rlm_core_with_queue(&double, Arc::clone(&responses));
    serve_processes(&double, &second_core);
    let second_session = second_core
        .session("processes-create-cold-reopen")
        .open()
        .await
        .expect("cold reopen");
    let second = second_session
        .send(TurnInput::text("start created definition"))
        .output()
        .await
        .expect("second turn");
    assert!(
        second.is_success(),
        "a created definition survives cold reopen: {second:?}"
    );
    assert!(responses.lock_recover().is_empty());
    assert_eq!(last_cell_finish(&second), Some(serde_json::json!(42)));
    let after_start = wait_edges(&double, |edges| {
        edges.iter().any(|edge| {
            edge.kind == "frame_environment"
                && edge.id == frame_id
                && edge.artifact_ref == module_ref
        })
    })
    .await;
    assert_eq!(
        frame_artifacts(&after_start),
        BTreeSet::from([module_ref.as_str()]),
        "the frame holds exactly the created module"
    );
}

/// FIG-3116: deleting the session that created a definition ends the frame
/// that held it, and the relay reclaims the module (ADR 0113 §3.1): nothing
/// else keeps a created definition alive.
#[tokio::test]
async fn created_definition_is_reclaimed_after_session_deletion() {
    let double =
        lash_restate_test::backend(0x3116_0002, lash_restate_test::ServerConfig::default())
            .await
            .expect("Restate double");
    let core = rlm_core(&double, vec![response(&create_definition_cell("made"))]);
    let session_id = "processes-create-deletion";
    let session = core.session(session_id).open().await.expect("session");
    let created = session
        .send(TurnInput::text("create definition"))
        .output()
        .await
        .expect("create turn");
    assert!(created.is_success(), "processes.create turn: {created:?}");
    let held = wait_edges(&double, |edges| {
        edges.len() == 1 && edges[0].kind == "frame_environment"
    })
    .await;
    let module_ref = held[0].artifact_ref.clone();
    let modules = double.lash_backend().module_artifacts();
    assert!(
        modules
            .get_module_artifact(&module_ref)
            .await
            .expect("read created module")
            .is_some(),
        "the frame holds the created module"
    );
    drop(session);

    let handler = double
        .open_handler(lash_core::AdmittedScope::session_delete(
            lash_core::SessionId::from(session_id),
        ))
        .await
        .expect("open the delete handler");
    let deletion = {
        struct HandlerExecution<'a> {
            administration: lash_core::SessionAdministration,
            scoped: lash_core::ScopedEffectController<'a>,
        }
        impl lash_core::SessionDeleteExecution for HandlerExecution<'_> {
            fn administration(&self) -> &lash_core::SessionAdministration {
                &self.administration
            }
            fn scoped<'run>(
                &'run self,
                _: lash_core::AdmittedScope,
            ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError>
            {
                Ok(self.scoped.clone())
            }
        }
        let execution = HandlerExecution {
            administration: core.session_administration().await,
            scoped: handler.scoped(),
        };
        let context = lash_core::SessionDeleteContext::from_execution(&execution, session_id)
            .expect("delete context");
        LashCore::delete_session(context).await
    };
    handler.close().await.expect("close the delete handler");
    assert!(
        matches!(deletion, Ok(lash::SessionDeletion::Deleted(_))),
        "the delete runs in the call: {deletion:?}"
    );

    let after = wait_edges(&double, |edges| {
        !edges.iter().any(|edge| edge.artifact_ref == module_ref)
    })
    .await;
    assert!(after.is_empty(), "no edge survives the session: {after:?}");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while modules
            .get_module_artifact(&module_ref)
            .await
            .expect("read module after deletion")
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the created module is reclaimed after its session is deleted");
}
