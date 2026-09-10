//! Cancel-mode witnesses on a controller-owned journal (FIG-635): the
//! after-step stop is re-observed at the step boundary under a
//! replay-deterministic identity; an escalated abort lands between journal
//! commands; a replaying owner honours the same request at the same identity.

use super::*;

#[tokio::test]
async fn durable_cancel_landing_during_llm_is_observed_after_the_journaled_run() {
    let recorder = RecordingEffectController::default()
        .with_cancel_after_llm()
        .with_controller_owned_replay();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("cancel while the model is running"),
            CancellationToken::new(),
            scoped_test_turn(&recorder, &TurnId::from("llm-cancel-boundary")),
        )
        .await
        .expect("cancelled turn");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert_eq!(
        recorder
            .records()
            .into_iter()
            .map(|record| (record.kind, record.replay_key))
            .collect::<Vec<_>>(),
        vec![
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.start_gate".to_string()
            ),
            (
                RuntimeEffectKind::LlmCall,
                "root:llm-cancel-boundary:1:0:llm_call:1".to_string()
            ),
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.after_llm.0".to_string()
            ),
        ],
        "the deployed LLM command must stay first within the iteration and the durable cancel observation must follow it"
    );
}

fn cancel_observation_sequence(
    recorder: &RecordingEffectController,
) -> Vec<(RuntimeEffectKind, String)> {
    recorder
        .records()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind,
                RuntimeEffectKind::PeekAwaitEvent | RuntimeEffectKind::LlmCall
            )
        })
        .map(|record| (record.kind, record.replay_key))
        .collect()
}

fn tool_attempt_count(recorder: &RecordingEffectController) -> usize {
    recorder
        .records()
        .into_iter()
        .filter(|record| record.kind == RuntimeEffectKind::ToolAttempt)
        .count()
}

#[tokio::test]
async fn after_step_cancel_on_a_controller_owned_journal_is_peeked_after_the_checkpoint() {
    let recorder = RecordingEffectController::default()
        .with_after_step_cancel()
        .with_controller_owned_replay();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EchoTool),
        mock_provider(Vec::new()),
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("use the tool, then stop after the step"),
            CancellationToken::new(),
            scoped_test_turn(&recorder, &TurnId::from("after-step-boundary")),
        )
        .await
        .expect("stopped turn");

    let evidence = match &turn.outcome {
        TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) => evidence.clone(),
        other => panic!("expected an after-step stop, got {other:?}"),
    };
    assert_eq!(evidence.request_id, "stop-after-step");
    assert_eq!(evidence.mode, crate::TurnCancelMode::AfterStep);
    assert_eq!(evidence.honoured_after_step, Some(0));
    assert_eq!(
        *recorder.llm_calls.lock_recover(),
        1,
        "the iteration that was running finishes; no further model call starts"
    );
    assert_eq!(
        tool_attempt_count(&recorder),
        2,
        "both tool calls of the closing step run to completion"
    );
    assert_eq!(turn.tool_calls.len(), 2);
    assert_eq!(
        cancel_observation_sequence(&recorder),
        vec![
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.start_gate".to_string()
            ),
            (
                RuntimeEffectKind::LlmCall,
                "root:after-step-boundary:1:0:llm_call:1".to_string()
            ),
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.after_llm.0".to_string()
            ),
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.escalation.after_llm.0".to_string()
            ),
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.after_step.0".to_string()
            ),
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.escalation.after_step.0".to_string()
            ),
        ],
        "an after-step request seen mid-iteration is re-observed at the step boundary, after the checkpoint, under a replay-deterministic identity"
    );
}

#[tokio::test]
async fn escalated_abort_on_a_controller_owned_journal_lands_between_journal_commands() {
    let recorder = RecordingEffectController::default()
        .with_after_step_cancel()
        .with_escalation_after_llm()
        .with_controller_owned_replay();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EchoTool),
        mock_provider(Vec::new()),
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("use the tool, then escalate"),
            CancellationToken::new(),
            scoped_test_turn(&recorder, &TurnId::from("escalated-after-llm")),
        )
        .await
        .expect("aborted turn");

    let evidence = match &turn.outcome {
        TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) => evidence.clone(),
        other => panic!("expected an escalated abort, got {other:?}"),
    };
    assert_eq!(evidence.request_id, "abort-escalated");
    assert_eq!(evidence.mode, crate::TurnCancelMode::Immediate);
    assert_eq!(evidence.honoured_after_step, None);
    assert_eq!(
        tool_attempt_count(&recorder),
        0,
        "an immediate abort on a controller-owned journal lands between journal commands: the tools of that step never start"
    );
    assert_eq!(
        cancel_observation_sequence(&recorder),
        vec![
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.start_gate".to_string()
            ),
            (
                RuntimeEffectKind::LlmCall,
                "root:escalated-after-llm:1:0:llm_call:1".to_string()
            ),
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.after_llm.0".to_string()
            ),
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.escalation.after_llm.0".to_string()
            ),
        ]
    );
}

#[tokio::test]
async fn replayed_owner_honours_the_after_step_stop_at_the_same_identity() {
    let recorder = RecordingEffectController::default()
        .with_after_step_cancel()
        .with_controller_owned_replay()
        .with_replay_by_key();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EchoTool),
        mock_provider(Vec::new()),
        host_with_effect_recorder(recorder.clone()),
    )
    .await;
    let first = runtime
        .run_turn_assembled(
            TurnInput::text("use the tool, then crash before the stop commits"),
            CancellationToken::new(),
            scoped_test_turn(&recorder, &TurnId::from("replayed-after-step")),
        )
        .await
        .expect("first owner stops");
    let first_evidence = match &first.outcome {
        TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) => evidence.clone(),
        other => panic!("expected an after-step stop, got {other:?}"),
    };
    assert_eq!(first_evidence.honoured_after_step, Some(0));
    let journaled_peeks = cancel_observation_sequence(&recorder);
    let llm_calls_before_replay = *recorder.llm_calls.lock_recover();

    // A new owner replays the same journal: no live cancel state, no canned
    // gate; every observation, including the after-step peek, comes back by
    // its recorded identity.
    let replaying = recorder.clone().without_canned_cancel();
    let mut replayed_runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EchoTool),
        mock_provider(Vec::new()),
        host_with_effect_recorder(replaying.clone()),
    )
    .await;
    let replayed = replayed_runtime
        .run_turn_assembled(
            TurnInput::text("use the tool, then crash before the stop commits"),
            CancellationToken::new(),
            scoped_test_turn(&replaying, &TurnId::from("replayed-after-step")),
        )
        .await
        .expect("replayed owner stops");
    assert_eq!(replayed.outcome, first.outcome);
    assert_eq!(
        *replaying.llm_calls.lock_recover(),
        llm_calls_before_replay,
        "replay never re-runs the model"
    );
    assert!(
        journaled_peeks
            .iter()
            .any(|(_, key)| key == "turn_cancel.after_step.0"),
        "the honouring peek is part of the journal the replay consumed"
    );
}
