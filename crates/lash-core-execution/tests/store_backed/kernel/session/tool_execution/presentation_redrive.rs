//! A redriven tool call replays its recorded presentation on the engine's
//! journal, however long the live call took.
//!
//! `complete_tool_call` journals the settled call's presentation as a
//! `PresentToolResult` effect keyed by `{call_id}:present`, and a redrive
//! re-issues it for the journal to serve. How long the call took is an
//! observation, not a recorded fact: a redriven call serves its journaled
//! attempt at once, so its settled duration differs from the live one. The
//! journal compares the redriven envelope with the recorded one, so a duration
//! in the envelope would refuse a healthy redrive with a replay hash conflict
//! — and the model would be shown that conflict in place of the tool's
//! result.

use serde_json::json;
use std::sync::{Arc, Mutex};

const CALL_ID: &str = "slow-call";
const SEED: u64 = 0x9e_5e_07;

/// A context over the handler's lent controller: each attempt builds a fresh
/// one, as a redrive does.
fn turn_context<'run>(
    backend: &crate::Backend,
    scoped: crate::ScopedEffectController<'run>,
) -> crate::RuntimeExecutionContext<'run> {
    crate::testing::TestExecutionContextBuilder::for_backend(backend)
        .session_id("presentation-session")
        .borrowed_effect_controller(scoped)
        .build()
        .into_runtime()
}

/// The settled call. Its duration lives on the observation argument, never in
/// the record.
fn settled() -> crate::tool_dispatch::ToolDispatchOutcome {
    crate::tool_dispatch::ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: Some(CALL_ID.to_string()),
            tool: "slow".to_string(),
            args: json!({}),
            output: crate::ToolCallOutput::success(json!({ "slow": "result" })),
        },
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
        captures: Vec::new(),
        triggers: Vec::new(),
    }
}

#[tokio::test]
async fn a_redriven_call_replays_its_presentation_whatever_its_duration() {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let live_return = Arc::new(Mutex::new(None));

    // The live pass: the call took 46 ms, then its turn crashed after the
    // presentation was journaled.
    let crashing: lash_restate_test::HandlerAttempt = {
        let backend = backend.clone();
        let live_return = Arc::clone(&live_return);
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let live_return = Arc::clone(&live_return);
            Box::pin(async move {
                let live = turn_context(&backend, scoped)
                    .complete_tool_call(
                        CALL_ID.to_string(),
                        crate::ToolId::new("timed"),
                        None,
                        settled(),
                        "test:call",
                        46,
                    )
                    .await
                    .expect("the live call presents");
                *live_return.lock().expect("the live-return cell") =
                    Some(live.completed.model_return);
                panic!("the turn crashes after its presentation is journaled");
            })
        })
    };
    // The redrive: the journaled attempt is served at once, however long the
    // live call took.
    let redrive: lash_restate_test::HandlerAttempt = {
        let backend = backend.clone();
        let live_return = Arc::clone(&live_return);
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let live_return = Arc::clone(&live_return);
            Box::pin(async move {
                let redriven = turn_context(&backend, scoped)
                    .complete_tool_call(
                        CALL_ID.to_string(),
                        crate::ToolId::new("timed"),
                        None,
                        settled(),
                        "test:call",
                        2,
                    )
                    .await
                    .expect("the redriven call is served its recorded presentation");
                assert_eq!(
                    redriven.completed.model_return,
                    live_return
                        .lock()
                        .expect("the live-return cell")
                        .clone()
                        .expect("the live attempt journaled its presentation"),
                    "the redrive is served the recorded presentation"
                );
                assert!(
                    redriven.completed.output.is_success(),
                    "the redriven call keeps its settled output"
                );
            })
        })
    };
    double
        .run_crashed_then_redriven(
            crate::AdmittedScope::turn("presentation-session", "presentation-turn"),
            crashing,
            redrive,
        )
        .await
        .expect("the live pass crashes and the redrive completes");
}
