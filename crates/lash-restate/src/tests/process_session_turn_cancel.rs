//! A `SessionTurn` process reads its cancellation through the engine's
//! recorded peek, never from the stop its execution lends (FIG-3673).

use super::*;

async fn run_session_turn(
    worker: &DurableProcessWorker,
    registry: &Arc<dyn ProcessRegistry>,
    registration: &ProcessRegistration,
    context: Arc<ReplayableRecordingContext>,
    lent_stop: tokio_util::sync::CancellationToken,
) -> Result<lash_core::ProcessRunOutcome, PluginError> {
    let controller = RestateRuntimeEffectController::with_options_for_test(
        context,
        RestateEffectControllerOptions::default().process_segment_drive(),
    );
    worker
        .run_process_segment_with_scoped_effect_controller(
            registration.clone(),
            ProcessExecutionContext::default(),
            lash_core::ProcessExecutionWriteAuthority::invocation(
                &registration.id,
                "session-turn-cancel-peek-execution",
            ),
            controller
                .process_scope_for_test(
                    recorded_process_admission(registry.as_ref(), &registration.id).await,
                )
                .expect("scope the session-turn process"),
            lent_stop,
            None,
        )
        .await
}

fn is_cancelled(outcome: &Result<lash_core::ProcessRunOutcome, PluginError>) -> bool {
    matches!(
        outcome,
        Ok(lash_core::ProcessRunOutcome::Terminal { output, .. })
            if output.terminal_status() == Some(lash_core::ProcessStatus::Cancelled)
    )
}

/// A committed cancel the first execution observed through its recorded peek
/// settles a redrive the same way, though the redrive's live state carries no
/// cancel and its lent stop is never fired: the redrive reads the peek it
/// recorded. And a fired lent stop with no committed cancel does not decide
/// the runner's branch: the peek answers from the recorded fact.
#[tokio::test]
pub(super) async fn a_session_turn_cancel_is_its_recorded_peek_not_its_lent_stop() {
    let registry = process_registry();
    let registration = rerunnable_session_turn_registration("p16-session-turn-cancel-peek");
    registry
        .register_process(registration.clone())
        .await
        .expect("register the session-turn process");
    let worker = recovery_worker(Arc::clone(&registry), memory_session_store_factory().await).await;

    let context = Arc::new(ReplayableRecordingContext::default());
    context.commit_process_cancel();
    let first = run_session_turn(
        &worker,
        &registry,
        &registration,
        Arc::clone(&context),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(
        is_cancelled(&first),
        "the recorded peek saw the committed cancel: {first:?}"
    );
    assert_eq!(context.process_cancel_peek_verdicts(), vec![true]);

    context.clear_process_cancel();
    context.start_replay();
    let replayed = run_session_turn(
        &worker,
        &registry,
        &registration,
        Arc::clone(&context),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(
        is_cancelled(&replayed),
        "the redrive reads its recorded peek, not live state: {replayed:?}"
    );
    assert_eq!(
        context.process_cancel_peek_verdicts(),
        vec![true],
        "a redrive consumes the recorded peek instead of recording a new one"
    );

    let fired = tokio_util::sync::CancellationToken::new();
    fired.cancel();
    let uncommitted = Arc::new(ReplayableRecordingContext::default());
    let _ = run_session_turn(
        &worker,
        &registry,
        &registration,
        Arc::clone(&uncommitted),
        fired,
    )
    .await;
    assert_eq!(
        uncommitted.process_cancel_peek_verdicts().first(),
        Some(&false),
        "a fired lent stop with no committed cancel does not take the cancelled branch"
    );
}
