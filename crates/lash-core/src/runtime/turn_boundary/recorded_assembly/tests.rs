//! The committed content is a pure function of recorded outcomes, checked with
//! the determinism harness (ADR 0105 §1; FIG-3672 P6).
//!
//! Each drive below is a turn in miniature over [`LocalTestCx`]: every model
//! call, tool call and code cell is a recorded operation whose body streams
//! observations through a [`TurnObserver`] from the execution side, and whose
//! outcome is the machine emissions for it. The drive folds those emissions
//! into a [`RecordedTurnAssembly`] and records the assembled turn's committed
//! content as its commit bytes.
//!
//! What this proves is narrow and deliberate: the fold is a pure function of
//! the recorded outcomes, whatever the scheduling of their delivery, and
//! folding in arrival order instead of declaration order would be caught. It
//! does not exercise the production driver or publisher; the real-runtime
//! tests (`tests/runtime/tests/commit_bytes.rs`) cover those, including a
//! host sink that blocks until released.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use super::RecordedTurnAssembly;
use crate::engine::testing::{
    DeterminismCheck, DeterminismFailure, FailureCause, LocalEngine, LocalTestCx, RunMode,
    TranscriptDivergence, TranscriptEntry,
};
use crate::llm::types::StreamBlockIdentity;
use crate::runtime::turn_observer::TurnObservations;
use crate::runtime::{RuntimeStreamEvent, TerminationPolicy, TurnObserver};
use crate::session_model::{SessionStreamEvent, TokenUsage};
use crate::testing::runtime_helpers::default_state;
use crate::{ToolCallOutput, TurnActivityId, TurnEvent, TurnFinish, TurnOutcome};

/// How the host end of the observation stream behaves.
#[derive(Clone, Copy)]
enum Host {
    /// Every observation is taken as soon as it is published.
    Prompt,
    /// The host takes nothing for a second, then catches up.
    StalledOneSecond,
}

/// The order in which the drive folds its parallel tool results.
#[derive(Clone, Copy)]
enum Fold {
    /// Declaration order, as the turn machine incorporates tool results.
    Declared,
    /// Completion order: the order an observation stream would deliver them.
    Arrival,
}

#[derive(Clone, Copy)]
struct MiniTurn {
    host: Host,
    fold: Fold,
}

const TOOLS: [&str; 3] = ["alpha", "beta", "gamma"];

fn usage(input_tokens: i64, output_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    }
}

fn publish_host_end(host: Host, mut observations: TurnObservations) {
    let spawned = std::thread::Builder::new()
        .name("mini-turn-host".to_string())
        .spawn(move || {
            if matches!(host, Host::StalledOneSecond) {
                std::thread::sleep(Duration::from_secs(1));
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if observations.try_take().is_none() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });
    assert!(spawned.is_ok(), "start the host end");
}

/// A model call's body: it streams deltas to the host as it goes, and its
/// outcome is what the machine emits for the recorded response.
async fn model_call(
    observer: TurnObserver,
    call: usize,
    deltas: usize,
    emissions: Vec<SessionStreamEvent>,
) -> Vec<SessionStreamEvent> {
    let block = StreamBlockIdentity::new(format!("text:{call}"), 0);
    for delta in 0..deltas {
        observer.publish(RuntimeStreamEvent::Session(SessionStreamEvent::TextDelta {
            content: format!("d{delta}"),
            block: block.clone(),
        }));
        observer.activity(
            TurnActivityId::new(block.id.clone()),
            TurnEvent::AssistantProseDelta {
                text: format!("d{delta}").into(),
                block: block.clone(),
            },
        );
        tokio::task::yield_now().await;
    }
    emissions
}

/// A tool call's body: it announces itself to the host, takes a while, and
/// its outcome is the machine's `ToolCall` emission for its result.
async fn tool_call(
    observer: TurnObserver,
    index: usize,
    name: &'static str,
) -> Vec<SessionStreamEvent> {
    observer.session(SessionStreamEvent::ToolCallStart {
        call_id: Some(format!("call-{index}")),
        name: name.to_string(),
        args: serde_json::json!({ "value": name }),
    });
    tokio::time::sleep(Duration::from_millis(((TOOLS.len() - index) * 3) as u64)).await;
    vec![SessionStreamEvent::ToolCall {
        call_id: Some(format!("call-{index}")),
        name: name.to_string(),
        args: serde_json::json!({ "value": name }),
        output: ToolCallOutput::success(serde_json::json!({ "echo": name })),
        duration_ms: 0,
    }]
}

async fn mini_turn(turn: MiniTurn, cx: &LocalTestCx) {
    let (observer, observations) = TurnObserver::unread();
    publish_host_end(turn.host, observations);
    let mut assembly = RecordedTurnAssembly::new();
    let fold = |assembly: &mut RecordedTurnAssembly, emissions: Vec<SessionStreamEvent>| {
        for event in emissions {
            assembly.record(&event);
            observer.session(event);
        }
    };

    let first = cx
        .op(
            "turn/llm/0",
            "llm_call",
            &serde_json::json!({ "call": 0 }),
            model_call(
                observer.clone(),
                0,
                24,
                vec![SessionStreamEvent::TokenUsage {
                    protocol_iteration: 0,
                    usage: usage(7, 2),
                    cumulative: usage(7, 2),
                }],
            ),
        )
        .await;
    fold(&mut assembly, first);

    let calls = TOOLS
        .iter()
        .enumerate()
        .map(|(index, name)| {
            cx.op(
                format!("turn/tool/{index}"),
                "tool_call",
                &serde_json::json!({ "call": index, "tool": name }),
                tool_call(observer.clone(), index, name),
            )
        })
        .collect::<Vec<_>>();
    match turn.fold {
        Fold::Declared => {
            for emissions in futures_util::future::join_all(calls).await {
                fold(&mut assembly, emissions);
            }
        }
        Fold::Arrival => {
            let mut arriving = calls;
            while !arriving.is_empty() {
                let (emissions, _, pending) = futures_util::future::select_all(arriving).await;
                fold(&mut assembly, emissions);
                arriving = pending;
            }
        }
    }

    let cell = cx
        .op(
            "turn/exec/0",
            "exec_code",
            &serde_json::json!({ "cell": 0 }),
            async {
                observer.activity(
                    TurnActivityId::new("code:0"),
                    TurnEvent::CodeBlockStarted {
                        language: "cell".to_string(),
                        code: "echo()".to_string(),
                        graph_key: None,
                    },
                );
                Vec::<SessionStreamEvent>::new()
            },
        )
        .await;
    assembly.note_code_execution();
    fold(&mut assembly, cell);

    let last = cx
        .op(
            "turn/llm/1",
            "llm_call",
            &serde_json::json!({ "call": 1 }),
            model_call(
                observer.clone(),
                1,
                16,
                vec![
                    SessionStreamEvent::TokenUsage {
                        protocol_iteration: 1,
                        usage: usage(11, 3),
                        cumulative: usage(18, 5),
                    },
                    SessionStreamEvent::TurnOutcome {
                        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                            text: "all three echoed".to_string(),
                        }),
                    },
                    SessionStreamEvent::Done,
                ],
            ),
        )
        .await;
    fold(&mut assembly, last);

    let turn = assembly.finish(
        default_state().to_snapshot(),
        None,
        None,
        &TerminationPolicy::default(),
    );
    cx.record_commit(&serde_json::json!({
        "tool_calls": turn.tool_calls,
        "omitted": turn.omitted,
        "token_usage": turn.token_usage,
        "outcome": turn.outcome,
        "had_tool_calls": turn.execution.had_tool_calls,
        "had_code_execution": turn.execution.had_code_execution,
        "assistant_output": turn.assistant_output.safe_text,
    }));
}

fn engine(turn: MiniTurn) -> LocalEngine<MiniTurn> {
    LocalEngine::new(
        move || turn,
        |turn: &MiniTurn, cx: &LocalTestCx| -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(mini_turn(*turn, cx))
        },
    )
}

const SEED: u64 = 0x3672_0006;

#[test]
fn commit_content_repeats_under_perturbed_scheduling_and_a_stalled_host() {
    let check = DeterminismCheck::new(SEED).perturbed_replays(6);
    let prompt = check
        .run(&engine(MiniTurn {
            host: Host::Prompt,
            fold: Fold::Declared,
        }))
        .unwrap_or_else(|failure| panic!("{failure}"));
    let stalled = check
        .run(&engine(MiniTurn {
            host: Host::StalledOneSecond,
            fold: Fold::Declared,
        }))
        .unwrap_or_else(|failure| panic!("{failure}"));

    assert!(
        prompt
            .transcript
            .entries
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::Commit { .. })),
        "the drive records its commit bytes"
    );
    prompt
        .transcript
        .compare(&stalled.transcript)
        .unwrap_or_else(|divergence| {
            panic!("a host that stalls for a second changed the commit: {divergence}")
        });
}

#[test]
fn folding_in_arrival_order_is_caught_by_the_check() {
    let failure: DeterminismFailure = DeterminismCheck::new(SEED)
        .perturbed_replays(6)
        .run(&engine(MiniTurn {
            host: Host::Prompt,
            fold: Fold::Arrival,
        }))
        .expect_err("an assembly folded in arrival order must diverge under perturbation");
    assert!(
        matches!(failure.mode, RunMode::Replay(_)),
        "the fresh run completes; a replay diverges: {failure}"
    );
    let FailureCause::Diverged(divergence) = &failure.cause else {
        panic!("expected a divergence, got {failure}");
    };
    assert!(
        matches!(
            &**divergence,
            TranscriptDivergence {
                expected: Some(TranscriptEntry::Commit { .. }),
                actual: Some(TranscriptEntry::Commit { .. }),
                ..
            }
        ),
        "the replay issues the same commands and commits different bytes: {failure}"
    );
}
