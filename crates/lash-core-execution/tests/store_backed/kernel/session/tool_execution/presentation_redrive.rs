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

use lash_sansio::core_support::ModelToolReturnCoreSupport as _;
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
            call_id: lash_core_execution::ToolCallId::fixture(CALL_ID),
            provider_call_id: None,
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
                        lash_core_execution::tool_dispatch::ToolCallIds {
                            call_id: lash_core_execution::ToolCallId::fixture(CALL_ID),
                            provider_call_id: None,
                        },
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
                        lash_core_execution::tool_dispatch::ToolCallIds {
                            call_id: lash_core_execution::ToolCallId::fixture(CALL_ID),
                            provider_call_id: None,
                        },
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

#[tokio::test]
async fn presentation_step_failure_feeds_fallback_to_next_step() {
    use lash_sansio::sync::MutexExt as _;
    let double = crate::support::kernel_double(SEED, Default::default()).await;
    let backend = double.lash_backend();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let failing: crate::plugin::ToolPresentationStep = {
        let calls = calls.clone();
        Arc::new(move |input| {
            calls.lock_recover().push("first".into());
            assert!(!input.previous.parts.is_empty());
            Box::pin(async {
                Err(crate::PluginError::Session(
                    "presentation fixture failed".into(),
                ))
            })
        })
    };
    let replacing: crate::plugin::ToolPresentationStep = {
        let calls = calls.clone();
        Arc::new(move |input| {
            calls.lock_recover().push("second".into());
            assert_eq!(
                input.previous.parts,
                vec![crate::ModelToolReturnPart::text(
                    "plugin session error: presentation fixture failed"
                )]
            );
            assert_eq!(input.previous.tool_name, "slow");
            Box::pin(async move {
                Ok(crate::ModelToolReturn::text(
                    input.context.tool_name,
                    "replacement after fallback",
                ))
            })
        })
    };
    let mut factories = crate::testing::test_code_protocol_factories();
    factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
        "fallback-chain",
        crate::plugin::PluginSpec::new()
            .with_presentation_step(failing)
            .with_presentation_step(replacing),
    )));
    let live_return = Arc::new(Mutex::new(None));
    let make_attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
        let backend = backend.clone();
        let factories = factories.clone();
        let live_return = live_return.clone();
        let calls = calls.clone();
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let factories = factories.clone();
            let live_return = live_return.clone();
            let calls = calls.clone();
            Box::pin(async move {
                let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
                    .session_id("presentation-session")
                    .plugin_factories(factories)
                    .borrowed_effect_controller(scoped)
                    .build()
                    .into_runtime();
                let outcome = context
                    .complete_tool_call(
                        lash_core_execution::tool_dispatch::ToolCallIds {
                            call_id: crate::ToolCallId::fixture(CALL_ID),
                            provider_call_id: None,
                        },
                        crate::ToolId::new("timed"),
                        None,
                        settled(),
                        "test:call",
                        1,
                    )
                    .await
                    .expect("presentation settles despite failed step");
                assert_eq!(
                    outcome.completed.model_return.parts,
                    vec![crate::ModelToolReturnPart::text(
                        "replacement after fallback"
                    )]
                );
                assert_eq!(
                    *calls.lock_recover(),
                    vec!["first", "second"],
                    "recorded replay skips both steps"
                );
                if crash {
                    *live_return.lock_recover() = Some(outcome.completed.model_return);
                    panic!("crash after recording presentation");
                }
                assert_eq!(
                    Some(outcome.completed.model_return),
                    *live_return.lock_recover()
                );
            })
        })
    };
    double
        .run_crashed_then_redriven(
            crate::AdmittedScope::turn("presentation-session", "fallback-turn"),
            make_attempt(true),
            make_attempt(false),
        )
        .await
        .unwrap();
    assert_eq!(*calls.lock_recover(), vec!["first", "second"]);
}
