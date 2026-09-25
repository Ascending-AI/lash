//! Commit-bytes pins for turn shapes a controller double drives: a code cell,
//! a cancel observed after the model call, and an after-step cancel honoured
//! at the step boundary. Captured before commit content moved onto the
//! driver's recorded state (FIG-3672 P6); a difference is a durable-format
//! change, not a new pin.
//!
//! Re-pinned once for a change of value and not of shape (FIG-3600): drive
//! admission mints a turn's root from its durable input, so a direct turn's
//! accepted input now carries its turn id in the existing optional
//! `source_key` field. That field is the only difference in the committed
//! bytes.

use super::*;
use crate::runtime_support::commit_pins::assert_commit_pins;

async fn run_pinned_controller_turn(
    recorder: RecordingEffectController,
    plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    tools: Arc<dyn lash_core::ToolProvider>,
    prompt: &str,
    turn_id: &str,
) -> (Vec<lash_core::RuntimeCommit>, AssembledTurn) {
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::testing::TestClock::new(1_000));
    let backend = memory_backend_with_clock(Arc::clone(&clock)).await;
    let store = unbound_recording_store_with_clock(&backend, clock).await;
    let session_id = format!("pin:{turn_id}");
    let mut runtime = crate::runtime_support::commit_pins::pinned_runtime(
        &session_id,
        plugins,
        tools,
        mock_provider(Vec::new()),
        host_with_effect_recorder(&backend, recorder.clone()),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    )
    .await;
    let sessions = RecordingSink::default();
    let activities = RecordingTurnEvents::default();
    let turn = runtime
        .stream_turn(
            TurnInput::text(prompt),
            TurnOptions::new(
                CancellationToken::new(),
                layered_scope(
                    &backend,
                    Arc::new(recorder.clone()),
                    AdmittedScope::turn(session_id.as_str(), TurnId::from(turn_id)),
                ),
            )
            .with_events(&sessions)
            .with_turn_events(&activities),
        )
        .await
        .expect("the turn assembles");
    (store.runtime_commits(), turn)
}

#[tokio::test]
async fn code_execution_turn_commits_the_pinned_bytes() {
    let (commits, turn) = Box::pin(run_pinned_controller_turn(
        RecordingEffectController::default(),
        vec![Arc::new(EffectControllerTestProtocolFactory {
            code_executor: Some(Arc::new(EffectControllerTestCodeExecutor)),
        })],
        Arc::new(EmptyTools),
        "run code",
        "commit-pin-code-execution",
    ))
    .await;
    assert!(turn.execution.had_code_execution);
    assert_commit_pins(
        "code execution",
        &commits,
        &["cd26ce1d113bcce09246f5790cc30c5dbf2fd8564635ab25e1aa80e523c8e74c"],
    );
}

#[tokio::test]
async fn cancel_observed_after_the_model_call_commits_the_pinned_bytes() {
    let (commits, turn) = Box::pin(run_pinned_controller_turn(
        RecordingEffectController::default()
            .with_cancel_after_llm()
            .with_controller_owned_replay(),
        Vec::new(),
        Arc::new(EmptyTools),
        "cancel while the model is running",
        "commit-pin-cancel-after-llm",
    ))
    .await;
    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert_commit_pins(
        "cancel after the model call",
        &commits,
        &["7b8316537a044646f724b1f132d3d41f33a364618cebf67fbcedb998ad84c062"],
    );
}

#[tokio::test]
async fn after_step_cancel_at_the_step_boundary_commits_the_pinned_bytes() {
    let (commits, turn) = Box::pin(run_pinned_controller_turn(
        RecordingEffectController::default()
            .with_after_step_cancel()
            .with_controller_owned_replay(),
        Vec::new(),
        Arc::new(EchoTool),
        "use the tool, then stop after the step",
        "commit-pin-after-step-cancel",
    ))
    .await;
    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert_eq!(turn.tool_calls.len(), 2);
    assert_commit_pins(
        "after-step cancel",
        &commits,
        &["f48b65b61585dff1328d44d4ee76b9c17238599be6c93f2ee57bc210bcfc62fe"],
    );
}
