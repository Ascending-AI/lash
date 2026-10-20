//! RLM frame evidence for ADR 0113: which frames, executions and host pins
//! hold a definition's module. Every law runs on the core's own node over
//! SQLite or PostgreSQL stores; the core's cleanup relay reclaims what an
//! ended frame held. The Python laws run this binary as their worker, so
//! it links the worker embedding (`worker`).

#![cfg(all(feature = "worker", feature = "sqlite", feature = "testing"))]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    reason = "backend-parametrized acceptance laws assert each step's result"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;
use lash::{LashCore, TurnInput, TurnOutput};
use lash_protocol_types::sync::MutexExt;

#[path = "artifact_referrers_evidence/fixture.rs"]
mod fixture;
use fixture::{Backend, Fixture};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    artifact_ref: String,
    kind: String,
    id: String,
}

/// Wait for the edge table to satisfy `predicate`. The bound is wall-clock
/// time, so a law that pauses the runtime's clock cannot spend it.
async fn wait_edges(fixture: &Fixture, predicate: impl Fn(&[Edge]) -> bool) -> Vec<Edge> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let edges = fixture.edges().await;
        if predicate(&edges) {
            return edges;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the edges never reached the awaited state: {edges:#?}"
        );
        fixture.settle().await;
    }
}

/// Move past the former deadline after a real store read. Manual polling
/// checks the pending wait without letting a paused clock auto-advance
/// PostgreSQL's connection timeouts.
async fn wait_edges_waits_for_condition_past_former_deadline(backend: Backend) {
    use futures_util::FutureExt as _;

    let fixture = Fixture::new(backend).await;
    let ready = std::sync::atomic::AtomicBool::new(false);
    let observed = tokio::sync::Notify::new();
    let waiting = wait_edges(&fixture, |edges| {
        observed.notify_one();
        ready.load(std::sync::atomic::Ordering::Relaxed) && edges.is_empty()
    });
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        _ = &mut waiting => panic!("the state wait returned before the condition held"),
        () = observed.notified() => {}
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(11)).await;
    assert!(
        waiting.as_mut().now_or_never().is_none(),
        "the state wait remains pending past the former deadline"
    );
    tokio::time::resume();
    ready.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(waiting.await.is_empty());
}

fn last_cell_finish(view: &lash::persistence::SessionReadView) -> Option<serde_json::Value> {
    view.to_snapshot()
        .session_graph
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            lash_core::SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(event),
            } => {
                let decoded = lash_protocol_rlm::decode_rlm_protocol_event(event)
                    .expect("recorded RLM protocol event decodes");
                let Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(cell)) =
                    decoded.as_deref()
                else {
                    return None;
                };
                match &cell.result {
                    lash_core::CellOutcome::Controlled {
                        control: lash_core::CellControl::Finish { value },
                        ..
                    } => value.inline().cloned(),
                    _ => None,
                }
            }
            _ => None,
        })
        .next_back()
}

/// `text`'s run, and when it switched frames, the frame task's own run
/// that follows it (FIG-5232): the output the switch's work ends in.
async fn send_through(session: &lash::LashSession, text: &str) -> lash::Result<TurnOutput> {
    let output = session.send(TurnInput::text(text)).output().await?;
    let lash::TurnOutcome::AgentFrameSwitch { frame_key, .. } = &output.outcome else {
        return Ok(output);
    };
    let follow_on = lash_core::runtime::durable::session_mail::frame_task_run(frame_key);
    let continued = session.attach_id(follow_on).output().await?;
    assert!(
        continued.is_success(),
        "the frame's task runs: {continued:?}"
    );
    Ok(continued)
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

fn rlm_core(fixture: &Fixture, responses: Vec<LlmResponse>) -> LashCore {
    rlm_core_with_queue(fixture, Arc::new(Mutex::new(VecDeque::from(responses))))
}

fn rlm_core_with_queue(fixture: &Fixture, queue: Arc<Mutex<VecDeque<LlmResponse>>>) -> LashCore {
    rlm_core_with_plugins(fixture, queue, Vec::new())
}

fn rlm_core_with_plugins(
    fixture: &Fixture,
    queue: Arc<Mutex<VecDeque<LlmResponse>>>,
    plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> LashCore {
    rlm_core_with_lifetime(
        fixture,
        queue,
        plugins,
        lash_execution_contracts::process::lifetime::session_or_starter,
    )
}

/// A core whose model-started processes take `lifetime`.
fn rlm_core_with_lifetime(
    fixture: &Fixture,
    queue: Arc<Mutex<VecDeque<LlmResponse>>>,
    plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    lifetime: fn(
        &lash_execution_contracts::process::StartCx,
    ) -> lash_execution_contracts::process::Lifetime,
) -> LashCore {
    rlm_core_in_dialect(
        fixture,
        queue,
        plugins,
        lifetime,
        lash_protocol_rlm::CellDialect::typescript(),
    )
}

#[expect(clippy::expect_used, reason = "test fixture validates its setup")]
fn rlm_core_in_dialect(
    fixture: &Fixture,
    queue: Arc<Mutex<VecDeque<LlmResponse>>>,
    plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    lifetime: fn(
        &lash_execution_contracts::process::StartCx,
    ) -> lash_execution_contracts::process::Lifetime,
    dialect: lash_protocol_rlm::CellDialect,
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
    let backend = fixture.backend.clone();
    let python = dialect.name() == "python";
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        dialect,
    );
    let factory = if python {
        factory.with_worker_service(python_process_workers())
    } else {
        factory
    };
    let builder = LashCore::rlm_builder(backend, factory);
    let builder = plugins
        .into_iter()
        .fold(builder, |builder, plugin| builder.plugin(plugin));
    builder
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("artifact-referrers")
                .cache_retention(lash::provider::CacheRetention::Short)
                .context_window_tokens(16_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lifetime,
                lash_core::runtime::env_policy::policy::from_run(|_| Ok(Vec::new())),
            ),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("artifact-referrers-worker"),
            lash::persistence::LeaseIncarnationId::new("artifact-referrers-boot"),
        ))
        .expect("RLM core")
}

async fn first_turn_continue_as_fences_its_initial_frame(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(
        &fixture,
        vec![
            response(
                "const old = async () => 5; await control.continueAs({ task: 'next frame' });",
            ),
            response("await control.finish('next frame');"),
        ],
    );
    let session = created_session(&core, "artifact-referrers-first-switch")
        .await
        .open()
        .await
        .expect("session");
    let switched = send_through(&session, "switch on first turn")
        .await
        .expect("switch turn");
    assert!(switched.is_success(), "first-turn switch: {switched:?}");
    // The session is resident from its current frame (ADR 0112), so the
    // initial frame is read back through the history pages.
    let history = core
        .session(
            lash::SessionId::parse("artifact-referrers-first-switch")
                .expect("nonblank host identity"),
        )
        .durable()
        .await
        .expect("durable session")
        .history(
            lash::persistence::HistoryAnchor::Head,
            lash::persistence::HistoryBudget {
                max_nodes: std::num::NonZeroU32::new(256).expect("nonzero node budget"),
                max_bytes: std::num::NonZeroU64::new(1 << 24).expect("nonzero byte budget"),
            },
        )
        .await
        .expect("read the session history");
    assert!(history.next.is_none(), "one page holds the whole ancestry");
    let mut frames = Vec::new();
    for node in history.nodes.iter().rev() {
        if !frames.contains(&node.frame_node_id) {
            frames.push(node.frame_node_id.clone());
        }
    }
    assert_eq!(frames.len(), 2, "the switch opens a second frame");
    let first_frame = frames[0].as_str();
    let after = wait_edges(&fixture, |edges| {
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

/// CELL-NO-PUBLISH / DOC-COST: unstarted functions have no definitions.
/// Once declarations are retained, N functions cost only the cell document.
#[tokio::test]
async fn unstarted_cell_functions_publish_no_definitions_or_extra_declarations() {
    let fixture = Fixture::new(Backend::Sqlite).await;
    fixture.record_publications();
    let code = |count: usize| {
        let definitions = (0..count)
            .map(|n| format!("const w{n} = async () => {n};"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{definitions} await control.finish('bound');")
    };
    let core = rlm_core(&fixture, vec![response(&code(1)), response(&code(8))]);
    let session = created_session(&core, "no-cell-publications")
        .await
        .open()
        .await
        .expect("session");
    assert!(
        session
            .send(TurnInput::text("one function"))
            .output()
            .await
            .expect("first turn")
            .is_success()
    );
    let first = fixture.publications();
    assert!(
        !first
            .iter()
            .any(|(namespace, _)| namespace == "process_definition"),
        "no function was started: {first:?}"
    );
    assert!(
        session
            .send(TurnInput::text("eight functions"))
            .output()
            .await
            .expect("next turn")
            .is_success()
    );
    let writes = fixture.publications();
    assert!(
        !writes
            .iter()
            .any(|(namespace, _)| namespace == "process_definition"),
        "no definitions: {writes:?}"
    );
    let document_writes: Vec<_> = writes[first.len()..]
        .iter()
        .filter(|(namespace, _)| namespace == "kernel_document")
        .collect();
    assert_eq!(
        document_writes.len(),
        1,
        "only the document is written: {document_writes:?}"
    );
    assert!(!document_writes[0].1.starts_with("function:"));
}

/// Own-cell and later-cell starts keep the same signature and frozen capture.
/// Both dialects use the ordinary function crossing, with no cell entries.
#[tokio::test]
async fn own_and_later_cell_starts_keep_the_signature_and_return_the_capture() {
    for (dialect, define, start) in [
        (
            lash_protocol_rlm::CellDialect::typescript(),
            "const limit = 3; const w = async (n: number) => limit; const quiet = async () => {};",
            "const h = await processes.start({ definition: w, args: { n: 1 } });
const empty = await processes.start({ definition: quiet });
const byFunction = await processes.list({ definition: w, status: \"any\" });
const byHandle = await processes.list({ definition_id: h.definition_id, status: \"any\" });
await control.finish({ h, empty, byFunction, byHandle });",
        ),
        (
            lash_protocol_rlm::CellDialect::python(),
            "limit = 3\nasync def w(n: int):\n    return limit\nasync def quiet():\n    pass\n",
            "h = await processes.start({'definition': w, 'args': {'n': 1}})\nempty = await processes.start({'definition': quiet})\nbyFunction = await processes.list({'definition': w, 'status': 'any'})\nbyHandle = await processes.list({'definition_id': h['definition_id'], 'status': 'any'})\nawait control.finish({'h': h, 'empty': empty, 'byFunction': byFunction, 'byHandle': byHandle})",
        ),
    ] {
        let fixture = Fixture::new(Backend::Sqlite).await;
        let response = |code: String| LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: format!("<{}>\n{code}\n</{}>", dialect.name(), dialect.name()),
                response_meta: None,
            }],
            ..Default::default()
        };
        let queue = Arc::new(Mutex::new(VecDeque::from([
            response(format!("{define}\n{start}")),
            response(start.to_owned()),
        ])));
        let core = rlm_core_in_dialect(
            &fixture,
            queue,
            Vec::new(),
            lash_execution_contracts::process::lifetime::detached,
            dialect,
        );
        let session = created_session(&core, "cell-start-signatures")
            .await
            .open()
            .await
            .expect("session");
        let mut signatures = Vec::new();
        for text in ["start now", "start saved"] {
            let output = session
                .send(TurnInput::text(text))
                .output()
                .await
                .expect("turn");
            assert!(output.is_success(), "{text}: {output:?}");
            let view = session.durable().read().await.expect("read").expect("head");
            let answer = last_cell_finish(&view).expect("the returned handle and listings");
            let handle = &answer["h"];
            let ids = |name: &str| {
                answer[name]
                    .as_array()
                    .expect("listing")
                    .iter()
                    .map(|row| row["process_id"].clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                ids("byFunction"),
                ids("byHandle"),
                "a function and its returned id select the same runs"
            );
            assert!(ids("byFunction").contains(&handle["process_id"]));
            let id = lash_core::ProcessDefinitionId::from_tagged_json(&handle["definition_id"])
                .expect("realized definition id");
            let definition = core
                .host_artifacts()
                .get_definition(&id)
                .await
                .expect("definition reads")
                .expect("the process holds it");
            signatures.push(definition.signature);
            let process = lash_core::process_id_from_handle_json(handle).expect("process handle");
            let end = core
                .processes()
                .await_output(&process)
                .await
                .expect("process result");
            let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = end
            else {
                panic!("settled process")
            };
            assert!(
                matches!(&output.outcome, lash::tools::ToolCallOutcome::Success(value) if value.to_json_value() == serde_json::json!(3)),
                "{output:?}"
            );
            let empty = lash_core::process_id_from_handle_json(&answer["empty"])
                .expect("the no-return function's handle");
            let empty = core
                .processes()
                .await_output(&empty)
                .await
                .expect("no-return function result");
            let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = empty
            else {
                panic!("settled no-return function")
            };
            assert!(
                matches!(&output.outcome, lash::tools::ToolCallOutcome::Success(value) if value.to_json_value().is_null()),
                "a start without a return normalizes its result to null: {output:?}"
            );
        }
        assert_eq!(
            signatures[0], signatures[1],
            "saved starts keep their signature"
        );
        core.shutdown().await.expect("stop the node");
    }
}

/// START-ONCE's cell boundary: cancellation of an unstarted function adds
/// neither a descriptor publication nor a definition edge to the frame.
#[tokio::test]
async fn a_cancelled_cell_with_unstarted_functions_adds_no_frame_definition_edge() {
    let fixture = Fixture::new(Backend::Sqlite).await;
    fixture.record_publications();
    let core = rlm_core(
        &fixture,
        vec![response("const w = async () => 3; await sleep(3600000);")],
    );
    let session = created_session(&core, "cancel-no-entry")
        .await
        .open()
        .await
        .expect("session");
    let handle = session
        .send(TurnInput::text("define and wait"))
        .await
        .expect("accepted turn");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !fixture.publications().iter().any(|(namespace, artifact)| {
        namespace == "kernel_document" && !artifact.starts_with("function:")
    }) {
        assert!(
            std::time::Instant::now() < deadline,
            "the cell document never published"
        );
        fixture.settle().await;
    }
    handle.cancel().await.expect("cancel turn");
    handle.outcome().await.expect("cancelled outcome");
    lash::testing::quiesce_artifact_cleanup(&core)
        .await
        .settle()
        .await
        .expect("execution cleanup");
    assert!(
        !fixture
            .publications()
            .iter()
            .any(|(namespace, _)| namespace == "process_definition")
    );
    assert!(
        !fixture
            .edges()
            .await
            .iter()
            .any(|edge| edge.kind == "frame_environment"),
        "the cancelled cell retained no definition document in the frame"
    );
    core.shutdown().await.expect("stop node");
}

/// A cell whose document lists library helpers: `map` reaches them.
fn helper_cell(answer: u32) -> String {
    format!(
        "const doubled = [1, 2].map((value) => value * 2); await control.finish(doubled[0] + {answer});"
    )
}

/// Wait until no execution holds a kernel document any more: each cell's
/// execution has settled and released what it held.
async fn wait_executions_released(fixture: &Fixture) -> Vec<Edge> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let edges = fixture.kernel_document_edges().await;
        if edges.iter().all(|edge| edge.kind != "execution") {
            return edges;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an execution still holds a kernel document: {edges:#?}"
        );
        fixture.settle().await;
    }
}

/// DECLARATIONS-RETAINED (FIG-6481): a cell writes only the declarations it
/// adds. Two cells in one frame list the same helpers: the first stores each
/// declaration once, held by the frame, and the second writes no
/// declaration pointer or edge, so no declaration blob either.
#[tokio::test]
async fn a_second_cell_with_unchanged_declarations_writes_no_declaration_row() {
    let fixture = Fixture::new(Backend::Sqlite).await;
    fixture.record_declaration_writes();
    let core = rlm_core(
        &fixture,
        vec![response(&helper_cell(1)), response(&helper_cell(2))],
    );
    let session = created_session(&core, "declarations-retained")
        .await
        .open()
        .await
        .expect("session");
    let first = session
        .send(TurnInput::text("first cell"))
        .output()
        .await
        .expect("first turn");
    assert!(first.is_success(), "the first cell: {first:?}");
    let held = wait_executions_released(&fixture).await;
    let first_writes = fixture.declaration_writes();
    assert!(
        first_writes
            .iter()
            .any(|write| write.starts_with("artifact_refs INSERT")),
        "the first cell stores the declarations it lists: {first_writes:#?}"
    );
    let second = session
        .send(TurnInput::text("second cell"))
        .output()
        .await
        .expect("second turn");
    assert!(second.is_success(), "the second cell: {second:?}");
    assert_eq!(
        wait_executions_released(&fixture).await,
        held,
        "the frame holds the same declarations after the second cell"
    );
    let second_writes = &fixture.declaration_writes()[first_writes.len()..];
    assert!(
        second_writes.is_empty(),
        "the second cell lists the same declarations and writes none of them: {second_writes:#?}"
    );
}

/// FIG-6481: the frame that retains its cells' declarations ends with its
/// session, and the relay reclaims every declaration with it: deleting the
/// session leaves no kernel-document pointer and no edge.
async fn a_deleted_session_leaves_no_declaration_row(backend: Backend) {
    let fixture = Fixture::new(backend).await;
    let core = rlm_core(&fixture, vec![response(&helper_cell(1))]);
    let session_id = "declarations-deleted";
    let session = created_session(&core, session_id)
        .await
        .open()
        .await
        .expect("session");
    let ran = session
        .send(TurnInput::text("cell"))
        .output()
        .await
        .expect("cell turn");
    assert!(ran.is_success(), "the cell: {ran:?}");
    let held = wait_executions_released(&fixture).await;
    assert!(
        held.iter()
            .any(|edge| edge.kind == "frame_environment"
                && edge.artifact_ref.starts_with("function:")),
        "the frame retains the cell's declarations: {held:#?}"
    );
    drop(session);
    let administration = core.session_administration().await;
    let deletion = LashCore::delete_session(
        administration
            .delete_context(session_id)
            .expect("delete context"),
    )
    .await;
    assert!(
        matches!(deletion, Ok(lash::SessionDeletion::Requested { .. })),
        "the delete runs in the call: {deletion:?}"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let edges = fixture.kernel_document_edges().await;
        let pointers = fixture.kernel_document_pointers().await;
        if edges.is_empty() && pointers.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the deleted session left kernel-document rows: {edges:#?} {pointers:#?}"
        );
        fixture.settle().await;
    }
}

/// FIG-5772: a saved function started as a process outlives the cell and
/// the session that started it. One cell binds a function, which the
/// session keeps as a saved function; a later cell starts it with
/// `processes.start`, which declares it in that cell's document under an
/// entry of its own; the session is deleted while the process sleeps, and
/// the process still finishes with the function's result.
async fn a_saved_function_started_as_a_process_finishes_after_its_session_is_deleted(
    backend: Backend,
) {
    saved_function_process(backend, lash_protocol_rlm::CellDialect::typescript(),
        "const factor = 2;\nasync function work(n: number) { await sleep(1500); return n * factor; }\nawait control.finish('bound');",
        "const run = await processes.start({ definition: work, args: { n: 21 } }); await control.finish(run);",
        serde_json::json!(42)).await;
}

/// FIG-5778 / K-FN-004: a Python saved function is a self-contained process definition;
/// its detached process finishes after the originating session is deleted.
async fn a_python_saved_function_process_outlives_its_session(backend: Backend) {
    saved_function_process(backend, lash_protocol_rlm::CellDialect::python(),
        "import asyncio\nfactor = 2\nasync def work(n: int = 21) -> int:\n    await asyncio.sleep(1.5)\n    return n * factor\nawait control.finish('bound')",
        "run = await processes.start({'definition': work, 'args': {}})\nawait control.finish(run)",
        serde_json::json!(42)).await;
}

async fn saved_function_process(
    backend: Backend,
    dialect: lash_protocol_rlm::CellDialect,
    define: &str,
    start: &str,
    expected: serde_json::Value,
) {
    let fixture = Fixture::new(backend).await;
    let response = |code| LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: format!("<{}>\n{code}\n</{}>", dialect.name(), dialect.name()),
            response_meta: None,
        }],
        ..Default::default()
    };
    let responses = Arc::new(Mutex::new(VecDeque::from(vec![
        response(define),
        response(start),
    ])));
    let core = rlm_core_in_dialect(
        &fixture,
        Arc::clone(&responses),
        Vec::new(),
        lash_execution_contracts::process::lifetime::detached,
        dialect,
    );
    let session_id = "saved-function-process";
    let session = created_session(&core, session_id)
        .await
        .open()
        .await
        .expect("session");
    let bound = session
        .send(TurnInput::text("bind the function"))
        .output()
        .await
        .expect("defining turn");
    assert!(bound.is_success(), "the defining turn: {bound:?}");
    let started = session
        .send(TurnInput::text("start it as a process"))
        .output()
        .await
        .expect("starting turn");
    assert!(started.is_success(), "the starting turn: {started:?}");
    let handle = last_cell_finish(
        &session
            .durable()
            .read()
            .await
            .expect("committed read")
            .expect("head"),
    )
    .expect("the cell answers with the handle");
    let process_id = handle
        .get("process_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| lash_core::ProcessId::parse(id).ok())
        .unwrap_or_else(|| panic!("a process handle names its process: {handle}"));
    drop(session);

    let administration = core.session_administration().await;
    let deletion = LashCore::delete_session(
        administration
            .delete_context(session_id)
            .expect("delete context"),
    )
    .await;
    assert!(
        matches!(deletion, Ok(lash::SessionDeletion::Requested { .. })),
        "the delete runs in the call: {deletion:?}"
    );

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(&process_id),
    )
    .await
    .expect("the process settles")
    .expect("the output reads");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settles with an output: {output:?}");
    };
    assert!(output.is_success(), "the process finishes: {output:?}");
    assert_eq!(output.value_for_projection(), expected);
}

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "artifact-referrers",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        )))
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}

macro_rules! tiered {
    ($($law:ident),* $(,)?) => {$(
        mod $law {
            #[tokio::test]
            async fn sqlite() {
                super::$law(super::Backend::Sqlite).await;
            }

            #[tokio::test]
            #[ignore = "requires PostgreSQL; select inside a pg16 gate"]
            async fn postgres() {
                super::$law(super::Backend::Postgres).await;
            }
        }
    )*};
}

tiered!(
    wait_edges_waits_for_condition_past_former_deadline,
    first_turn_continue_as_fences_its_initial_frame,
    a_deleted_session_leaves_no_declaration_row,
    a_saved_function_started_as_a_process_finishes_after_its_session_is_deleted,
    a_python_saved_function_process_outlives_its_session,
);

#[test]
fn postgres_variants_never_pass_without_a_database_url() {
    let executable = std::env::current_exe().expect("test executable");
    let law = "wait_edges_waits_for_condition_past_former_deadline::postgres";
    for url in [None, Some(""), Some(" \t ")] {
        let mut command = std::process::Command::new(&executable);
        command
            .args(["--exact", law, "--include-ignored", "--nocapture"])
            .env_remove("LASH_POSTGRES_DATABASE_URL");
        if let Some(url) = url {
            command.env("LASH_POSTGRES_DATABASE_URL", url);
        }
        let output = command.output().expect("run PostgreSQL variant");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("running 1 test"), "{stdout}\n{stderr}");
        assert!(
            !output.status.success() && stdout.contains("0 passed; 1 failed"),
            "{law} with URL {url:?} passed vacuously: {stdout}\n{stderr}"
        );
        assert!(
            stderr.contains("LASH_POSTGRES_DATABASE_URL"),
            "{stdout}\n{stderr}"
        );
    }
}

/// What the worker of a host that names Python alone runs.
fn python_process_embedding(
    _tuning: &lash::vm::WorkerTuning,
) -> Result<lash::vm::WorkerEmbedder, lash::vm::WorkerEmbedError> {
    lash::vm::WorkerEmbedder::new().dialect(lash::vm::python_worker_dialect())
}

// This support entry becomes a worker only when the pool reexecs it.
#[test]
fn python_saved_function_worker_entry() {
    if lash::vm::worker_entry(&python_process_embedding).expect("Python worker") {
        std::process::exit(0);
    }
}

fn python_process_workers() -> lash::vm::WorkerService {
    let mut entry = lash::vm::WorkerEntry::reexec().expect("this test binary");
    entry.args = vec![
        "--exact".into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
        "--".into(),
        "python_saved_function_worker_entry".into(),
    ];
    lash::vm::WorkerService::new(lash::vm::WorkerPoolConfig::codemode(entry))
}
