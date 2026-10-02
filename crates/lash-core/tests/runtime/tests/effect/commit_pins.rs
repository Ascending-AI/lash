//! Commit-bytes pins for turn shapes a controller double executes: a code cell,
//! a cancel observed after the model call, and an after-step cancel honoured
//! at the step boundary. Captured before commit content moved onto the
//! driver's recorded state (FIG-3672 P6). During the pre-1.0 version freeze,
//! shapes change in place and pins are regenerated.
//!
//! `just runtime-commit-pins` proves each delta by reversing named serialized
//! shape changes until the complete previous digest matches. FIG-4839 covers
//! FIG-2002's removed policy session id and FIG-4655's output-token limits;
//! every other committed byte retains its check.
//!
//! Re-pinned once for a change of value and not of shape (FIG-3600): shift
//! admission mints a turn's run from its durable input, so a direct turn's
//! accepted input now carries its turn id in the existing optional
//! `source_key` field. That field is the only difference in the committed
//! bytes.
//!
//! Re-pinned for FIG-4037: each physical turn commit now carries its typed
//! `outcome`. Removing that field from the masked commit reproduces each old
//! digest; every other committed byte is unchanged.
//!
//! The two cancelled pins were re-pinned for ADR 0116 (FIG-4054): `batch` is
//! protocol sugar, so the test protocol registers no `batch` tool and the
//! committed tool state, which recorded that registration, is now empty. The
//! code-cell pin, whose protocol never registered `batch`, is unchanged.
//!
//! The two cancelled pins were re-pinned for ADR 0122 (FIG-4113): lash keeps
//! no partial of a stopped turn, so its commit no longer carries the
//! sealed-partial field. Removing that field from each ADR 0116 pin's masked
//! commit reproduces the new digest; the code-cell pin is unchanged.

//! Re-pinned for FIG-4745 under the version freeze: removing the namespace
//! format headers reproduces the cancelled-turn digests. The code-cell fixture
//! also materializes its factory ID, `test_protocol`, before callbacks;
//! removing that empty namespace reproduces its previous digest.

use super::*;
use crate::runtime_support::commit_pins::assert_commit_pins;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_f460;

async fn run_pinned_controller_turn(
    recorder: RecordingEffectController,
    plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    tools: Arc<dyn lash_core::ToolProvider>,
    prompt: &str,
    turn_id: &str,
) -> (Vec<lash_core::RuntimeCommit>, AssembledTurn) {
    let double = kernel_double(
        SEED,
        lash_restate_test::ServerConfig {
            start_time_ms: 1_000,
            ..lash_restate_test::ServerConfig::default().time(lash_restate_test::TimeMode::Manual)
        },
    )
    .await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let session_id = format!("pin:{turn_id}");
    let mut runtime = crate::runtime_support::commit_pins::pinned_runtime(
        &session_id,
        plugins,
        tools,
        mock_provider(Vec::new()),
        host_with_effect_recorder(&backend, recorder.clone()),
        store.clone() as Arc<dyn lash_core::RuntimeStore>,
    )
    .await;
    let sessions = RecordingSink::default();
    let activities = RecordingTurnEvents::default();
    let handler = double
        .open_handler(AdmittedScope::turn(
            lash_core::SessionId::fixture(session_id.as_str()),
            TurnId::fixture(turn_id),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .execute_turn(
            TurnInput::text(prompt),
            TurnOptions::new(
                CancellationToken::new(),
                lash_core::testing::LayeredEffectHost::layer_scoped(
                    handler.scoped(),
                    Arc::new(recorder.clone()),
                )
                .expect("layer the turn's scoped controller"),
            )
            .with_events(&sessions)
            .with_turn_events(&activities),
        )
        .await
        .expect("the turn assembles");
    handler.close().await.expect("close the turn's handler");
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
        &["40fdc84564544a36de50b9177807ae69ad228ffbd9cc387465f9f313ee772445"],
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
        &["fd5b77265a332987d98272b66ed24396168d59bf60480301bda96684c5b124bc"],
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
        &["f8199c772d2c414324efccc01c6d11b070e90cf03ec069902d47e0eb3849cefa"],
    );
}
