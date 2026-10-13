//! FIG-6092: aggregate callback deadlines refuse and replay on SQLite.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use lash_core::llm::types::{LlmOutputPart, LlmResponse};
use lash_core::plugin::{
    BeforeToolDecision, PluginDeclaration, PluginError, PluginSpec, StaticPluginFactory,
};
use lash_core::testing::runtime_helpers::{EchoTool, MockCall, mock_provider};
use lash_durable_test::SimClock;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

fn answer(parts: Vec<LlmOutputPart>) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts,
            ..Default::default()
        }),
    }
}

fn core(
    backend: &lash::Backend,
    plugin: Arc<dyn lash_core::plugin::PluginFactory>,
    tool: bool,
    boot: &str,
) -> lash::LashCore {
    let parts = if tool {
        vec![LlmOutputPart::ToolCall {
            call_id: "checked".into(),
            tool_name: "echo_tool".into(),
            input_json: "{\"value\":\"hello\"}".into(),
            replay: None,
        }]
    } else {
        vec![LlmOutputPart::Text {
            text: "done".into(),
            response_meta: None,
        }]
    };
    lash::LashCore::standard_builder(backend.clone())
        .tools(lash::hook_key!("tools"), Arc::new(EchoTool))
        .plugin(plugin)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(
            lash::ExecutionBudgets::new(lash::ExecutionBudgetsConfig {
                tool_check_phase: Duration::from_secs(7),
                turn_hook_phase: Duration::from_secs(12),
                ..lash::ExecutionBudgetsConfig::recommended()
            })
            .unwrap(),
        )
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            mock_provider(vec![
                answer(parts),
                answer(vec![LlmOutputPart::Text {
                    text: "done".into(),
                    response_meta: None,
                }]),
            ])
            .into_handle(),
            served::metadata(),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("phase-expiry"),
            lash::persistence::LeaseIncarnationId::new(boot),
        ))
        .unwrap()
}

async fn prove(tool: bool, denied: bool) {
    let clock = SimClock::new();
    let stores = sim::memory(clock.clone()).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let invoked = Arc::new(AtomicUsize::new(0));
    let mut spec = PluginSpec::new();
    if tool {
        let time = clock.clone();
        spec = spec.with_tool_args_transform(
            lash::hook_key!("transform"),
            Arc::new(move |input| {
                let time = time.clone();
                Box::pin(async move {
                    time.advance_by(2_000).await;
                    Ok(input.current)
                })
            }),
        );
        let count = invoked.clone();
        let time = clock.clone();
        spec = spec.with_tool_args_check(
            lash::hook_key!("allow"),
            Arc::new(move |_| {
                let count = count.clone();
                let time = time.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    time.advance_by(4_000).await;
                    Ok(BeforeToolDecision::Allow)
                })
            }),
        );
        let count = invoked.clone();
        if denied {
            let count = invoked.clone();
            spec = spec.with_tool_args_check(
                lash::hook_key!("deny"),
                Arc::new(move |_| {
                    let count = count.clone();
                    Box::pin(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(BeforeToolDecision::Deny(lash_core::ToolFailure::runtime(
                            lash_core::ToolFailureClass::InvalidRequest,
                            "policy_refused",
                            "policy denied the call",
                        )))
                    })
                }),
            );
        }
        let signal = entered.clone();
        let gate = release.clone();
        spec = spec.with_tool_args_check(
            lash::hook_key!("missing"),
            Arc::new(move |_| {
                let count = count.clone();
                let signal = signal.clone();
                let gate = gate.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    signal.notify_one();
                    gate.notified().await;
                    Err(PluginError::Session("released unanswered check".into()))
                })
            }),
        );
    } else {
        let count = invoked.clone();
        let signal = entered.clone();
        let gate = release.clone();
        spec = spec.with_checkpoint(
            lash::hook_key!("missing"),
            Arc::new(move |ctx| {
                let count = count.clone();
                let signal = signal.clone();
                let gate = gate.clone();
                Box::pin(async move {
                    if ctx.checkpoint != lash_core::CheckpointKind::BeforeCompletion {
                        return Ok(Default::default());
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    signal.notify_one();
                    gate.notified().await;
                    Err(PluginError::Session(
                        "released unanswered checkpoint".into(),
                    ))
                })
            }),
        );
    }
    let plugin: Arc<dyn lash_core::plugin::PluginFactory> = Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("phase-law"),
        spec,
    ));
    let backend = served::backend(Arc::new(stores.clone()));
    let first = core(&backend, plugin.clone(), tool, "first");
    let id = lash::SessionId::from("phase-law-session");
    let run = lash::TurnId::parse("phase-law-run").unwrap();
    let session = first
        .session(id.clone())
        .create(lash::SessionCreation::root(served::spec(8)))
        .await
        .unwrap();
    let handle = session
        .send(lash::TurnInput::text("check phase"))
        .id(run.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("callback entered");
    clock.advance_by(if tool { 1_000 } else { 12_000 }).await;
    // Poll the deadline before unblocking the old implementation, so the red
    // side settles deterministically too, without a wall-clock sleep.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    release.notify_one();
    let output = tokio::time::timeout(Duration::from_secs(10), handle.output())
        .await
        .expect("turn settles")
        .unwrap();
    let recorded = serde_json::to_value(&output.result).unwrap();
    let text = recorded.to_string();
    assert!(
        text.contains("control_phase_expired"),
        "typed expiry missing: {text}"
    );
    assert!(
        text.contains("phase-law") && text.contains("missing"),
        "expiry names unanswered callback: {text}"
    );
    assert!(
        text.contains(if tool { "7000" } else { "12000" }),
        "expiry retains configured bound: {text}"
    );
    if tool {
        let lash_core::ToolCallOutcome::Failure(failure) =
            &output.result.tool_calls[0].output.outcome
        else {
            panic!("expired check did not refuse");
        };
        if denied {
            assert_eq!(
                failure.code, "policy_refused",
                "expiry must not change reduction order"
            );
        } else {
            let Some(lash_core::ToolFailureCause::ControlPhaseExpired { expiry }) =
                failure.cause.as_deref()
            else {
                panic!("typed tool expiry missing");
            };
            assert_eq!(expiry.phase, "tool_args");
            assert_eq!(expiry.bound_ms, 7_000);
        }
        let evidence = failure
            .raw
            .as_ref()
            .expect("complete check evidence")
            .to_json_value();
        let replies = evidence.as_array().expect("recorded verdicts");
        assert_eq!(replies.len(), if denied { 3 } else { 2 });
        assert!(
            replies
                .iter()
                .any(|reply| reply["callback"]["key"] == "tool_args_check:allow"
                    && reply["verdict"]["verdict"] == "allow")
        );
        assert!(replies.iter().any(
            |reply| reply["callback"]["key"] == "tool_args_check:missing"
                && reply["verdict"]["value"]["cause"]["kind"] == "control_phase_expired"
        ));
    }
    if !tool {
        let lash::TurnOutcome::Stopped(lash::TurnStop::RuntimeError { cause }) =
            &output.result.outcome
        else {
            panic!("checkpoint did not fail through its hook path");
        };
        let expiry = cause
            .control_phase_expired
            .as_ref()
            .expect("typed checkpoint expiry");
        assert_eq!(expiry.phase, "checkpoint");
        assert_eq!(expiry.bound_ms, 12_000);
        assert_eq!(expiry.callback, "checkpoint:missing");
    }
    let calls = invoked.load(Ordering::SeqCst);
    assert_eq!(calls, if tool { 2 + usize::from(denied) } else { 1 });
    first.shutdown().await.unwrap();
    let reopened = stores.reopen_with_clock(clock).await.unwrap();
    let second = core(&served::backend(Arc::new(reopened)), plugin, tool, "second");
    let session = second.session(id).durable().await.unwrap();
    let replay = session.attach_id(run).output().await.unwrap();
    assert_eq!(
        serde_json::to_value(&replay.result.outcome).unwrap(),
        recorded["outcome"]
    );
    assert_eq!(
        serde_json::to_value(&replay.result.tool_calls).unwrap(),
        recorded["tool_calls"]
    );
    assert_eq!(
        invoked.load(Ordering::SeqCst),
        calls,
        "replay invoked a completed phase"
    );
    second.shutdown().await.unwrap();
}

/// CHECK-PHASE-EXPIRY-REFUSES: one Allow and one unanswered check refuse,
/// retaining typed expiry, and reopen invokes neither callback again.
#[tokio::test]
async fn check_phase_expiry_refuses() {
    prove(true, false).await;
    prove(true, true).await;
}

/// TURN-HOOK-PHASE-EXPIRY-FAILS-TYPED: the checkpoint fails through the
/// hook error path, retaining typed expiry across reopen.
#[tokio::test]
async fn turn_hook_phase_expiry_fails_typed() {
    prove(false, false).await;
}
