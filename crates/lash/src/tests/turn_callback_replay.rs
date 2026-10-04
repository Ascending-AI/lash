//! FIG-4921: accepted callbacks restore session changes without rerunning hooks.

use super::*;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Before,
    After,
    Checkpoint,
    None,
}

struct SearchTool;
fn search_tool() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:hook-search",
            "hook_search",
            "replay probe",
            serde_json::json!({"type": "object"}),
            serde_json::json!({}),
        )
        .unwrap(),
        "hook_search",
    )
}
#[async_trait]
impl ToolProvider for SearchTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![search_tool().manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "hook_search").then(|| Arc::new(search_tool().contract()))
    }
    async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        panic!("the fixture never calls a tool")
    }
}

type Reads = Arc<StdMutex<Option<Arc<dyn lash_core::plugin::SessionReadService>>>>;
fn contribution(
    server: &lash_restate_test::RestateTestServer,
    calls: &AtomicUsize,
) -> lash_core::plugin::SessionContributions {
    calls.fetch_add(1, Ordering::SeqCst);
    server.crash_on(
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: lash_restate_test::protocol::MessageType::ProposeRunCompletionAck,
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .times(u32::MAX),
    );
    lash_core::plugin::SessionContributions {
        tool_membership: vec![lash_core::plugin::ToolMembershipContribution {
            tool_id: "tool:hook-search".into(),
            present: false,
        }],
        graph_appends: vec![lash_core::AppendSessionNodesRequest {
            operation_id: "accepted-hook-append".into(),
            nodes: vec![lash_core::SessionAppendNode::plugin(
                "test.accepted-hook",
                serde_json::json!({"accepted": true}),
            )],
            requires_ancestor_node_id: None,
        }],
    }
}

fn core_over(
    double: &RestateTestBackend,
    phase: Phase,
    calls: &Arc<AtomicUsize>,
    reads: &Reads,
) -> LashCore {
    let calls = Arc::clone(calls);
    let reads = Arc::clone(reads);
    let server = double.server().clone();
    let spec = lash_core::facade_support::PluginSpec::new();
    let spec = match phase {
        Phase::None => spec,
        Phase::Before => spec.with_before_turn(
            crate::hook_key!("contribute"),
            Arc::new(move |ctx| {
                *reads.lock_recover() = Some(ctx.sessions);
                let session = contribution(&server, &calls);
                Box::pin(async move {
                    Ok(lash_core::plugin::TurnContributions {
                        session,
                        ..Default::default()
                    })
                })
            }),
        ),
        Phase::After => spec.with_after_turn(
            crate::hook_key!("contribute"),
            Arc::new(move |ctx| {
                *reads.lock_recover() = Some(ctx.sessions);
                let session = contribution(&server, &calls);
                Box::pin(async move {
                    Ok(lash_core::plugin::AfterTurnContributions {
                        session,
                        ..Default::default()
                    })
                })
            }),
        ),
        Phase::Checkpoint => spec.with_checkpoint(
            crate::hook_key!("contribute"),
            Arc::new(move |ctx| {
                let session = if ctx.checkpoint == lash_core::CheckpointKind::BeforeCompletion {
                    *reads.lock_recover() = Some(ctx.sessions);
                    contribution(&server, &calls)
                } else {
                    Default::default()
                };
                Box::pin(async move {
                    Ok(lash_core::plugin::TurnContributions {
                        session,
                        ..Default::default()
                    })
                })
            }),
        ),
    };
    let plugin = StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("turn-contribution-replay"),
        spec,
    );
    LashCore::standard_builder(double.lash_backend())
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(
            crate::testing::TestProvider::builder()
                .kind("turn-contribution-replay")
                .complete(move |request| async move {
                    let offered = request.tools.iter().any(|tool| tool.name == "hook_search");
                    assert_eq!(
                        offered,
                        matches!(phase, Phase::After | Phase::Checkpoint),
                        "the first model request observes accepted before-turn membership on replay"
                    );
                    Ok(text_response("done"))
                })
                .build()
                .into_handle(),
            mock_llm_profile_spec(),
        )
        .tools(Arc::new(SearchTool))
        .plugin(Arc::new(plugin))
        .build(crate::testing::runtime_lease_owner())
        .unwrap()
}

async fn cold_replay(phase: Phase) -> Result<()> {
    let double = lash_restate_test::backend(
        0x4921,
        ServerConfig::default().protocol(lash_restate_test::ProtocolVersion::V7),
    )
    .await
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let reads: Reads = Arc::default();
    let core = core_over(&double, phase, &calls, &reads);
    let session_id = SessionId::fixture(format!("accepted-turn-contribution-{phase:?}"));
    let session = core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let first = Arc::new(StdMutex::new(Some(core)));
    let dropped = Arc::new(AtomicBool::new(false));
    assert!(double.server().on_crash({
        let first = Arc::clone(&first);
        let dropped = Arc::clone(&dropped);
        let reads = Arc::clone(&reads);
        Arc::new(move |_: &str| {
            let Some(core) = first.lock_recover().take() else { return; };
            let reads = reads.lock_recover().take().expect("the hook's read service");
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
                    .block_on(async move {
                        let snapshot = reads.snapshot_current().await.unwrap();
                        let tools = reads.tool_state(&snapshot.session_id).await.unwrap();
                        assert!(tools.get(&"tool:hook-search".into()).unwrap().is_member(),
                            "membership is invisible before acknowledgement");
                        assert!(!snapshot.session_graph.nodes.iter().any(|node| matches!(&node.payload,
                            lash_core::SessionNodePayload::Plugin { plugin_type, .. } if plugin_type == "test.accepted-hook")),
                            "graph appends are invisible before acknowledgement");
                        drop(reads);
                        drop(core);
                    });
            }).join().unwrap();
            dropped.store(true, Ordering::SeqCst);
        })
    }));
    drop(session.send(TurnInput::text("answer")).await?);
    drop(session);
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the worker dies after the hook result is stored");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let turn = double
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTurn/"))
        .expect("the turn invocation");
    assert!(
        double
            .server()
            .journal(&turn.id)
            .unwrap()
            .iter()
            .any(|entry| {
                entry.run_completion().is_some_and(|result| {
                    result.is_ok_and(|bytes| {
                        String::from_utf8_lossy(&bytes).contains("test.accepted-hook")
                    })
                })
            }),
        "the complete callback output is durable before the worker restarts"
    );
    let double = double.restart().await.unwrap();
    let second = core_over(&double, phase, &calls, &reads);
    double.server().clear_crashes();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let runs = double.server().invocations();
            for run in &runs {
                if run.status == "paused" {
                    double.server().resume(&run.id);
                }
            }
            if runs
                .iter()
                .any(|run| run.target.starts_with("LashTurn/") && run.status == "completed")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cold replay commits the turn");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "accepted callbacks never run again"
    );
    let durable = second.session(session_id.clone()).durable().await?;
    let history = durable_history(&durable).await?;
    let records = history.iter().filter(|node| matches!(&node.record.payload,
        lash_core::SessionNodePayload::Plugin { plugin_type, .. } if plugin_type == "test.accepted-hook")).count();
    assert_eq!(
        records, 1,
        "the accepted append reaches the durable turn exactly once"
    );
    // A turn's contributions extend one graph, never a replica of its read tail.
    for pair in history.windows(2) {
        assert_eq!(
            pair[0].record.parent_node_id.as_ref(),
            Some(&pair[1].record.node_id)
        );
    }
    assert_eq!(
        history
            .iter()
            .filter(|node| node.record.message().is_some())
            .count(),
        2,
        "the input and response persist once each"
    );
    let store = lash_core::runtime::live_session_view(&second.store_factory, &session_id)
        .await?
        .unwrap();
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .unwrap()
    .state;
    assert!(
        !state
            .tool_state_snapshot()
            .unwrap()
            .get(&"tool:hook-search".into())
            .unwrap()
            .is_member(),
        "accepted membership survives the final commit"
    );
    let leaf = history[0].record.node_id.clone();
    if phase == Phase::After {
        assert!(
            matches!(&history[0].record.payload,
            lash_core::SessionNodePayload::Plugin { plugin_type, .. } if plugin_type == "test.accepted-hook"),
            "the terminal append follows its turn's response"
        );
    }
    drop(durable);
    drop(second);
    let third = core_over(&double, Phase::None, &calls, &reads);
    let session = third.session(session_id.clone()).open().await?;
    session
        .send(TurnInput::text("next answer"))
        .output()
        .await?;
    let history = durable_history(&session.durable()).await?;
    for pair in history.windows(2) {
        assert_eq!(
            pair[0].record.parent_node_id.as_ref(),
            Some(&pair[1].record.node_id)
        );
    }
    assert_eq!(
        history
            .iter()
            .filter(|node| node.record.node_id == leaf)
            .count(),
        1,
        "the next turn extends the accepted leaf instead of replicating its read tail"
    );
    assert_eq!(
        history
            .iter()
            .filter(|node| node.record.message().is_some())
            .count(),
        4
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(session);
    drop(third);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_after_turn_append_survives_cold_replay_without_rerunning_the_hook()
-> Result<()> {
    cold_replay(Phase::After).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_before_turn_membership_and_appends_survive_cold_replay() -> Result<()> {
    cold_replay(Phase::Before).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_checkpoint_membership_and_appends_survive_cold_replay() -> Result<()> {
    cold_replay(Phase::Checkpoint).await
}
