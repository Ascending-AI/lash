use super::*;

/// One live-then-replayed run of the sleeping lashlang process under a
/// process-segment drive controller over `context` (FIG-3673).
async fn drive_sleeping_process(
    process_id: &ProcessId,
    registration: &ProcessRegistration,
    context: &Arc<ReplayableRecordingContext>,
    execution_write_authority: &lash_core::ProcessExecutionWriteAuthority,
) -> Result<lash_core::ProcessRunOutcome, HandlerError> {
    let (registry, continuations) = process_stores();
    registry
        .register_process(registration.clone())
        .await
        .expect("register sleeping process");
    let worker = recovery_worker(Arc::clone(&registry), memory_session_store_factory().await).await;
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(worker)),
        Arc::clone(&registry),
        continuations,
    );
    let controller = RestateRuntimeEffectController::with_options_for_test(
        Arc::clone(context),
        RestateEffectControllerOptions::default().process_segment_drive(),
    );
    workflow
        .run_registration_for_test(
            registration.clone(),
            ProcessExecutionContext::default()
                .with_execution_write_authority(execution_write_authority.clone()),
            controller
                .process_scope_for_test(durable_admission(&ExecutionScope::process(process_id)))
                .expect("sleeping process scope"),
            0,
            None,
        )
        .await
}

fn is_cancelled_terminal(outcome: &lash_core::ProcessRunOutcome) -> bool {
    matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if output.terminal_status() == Some(lash_core::ProcessStatus::Cancelled)
    )
}

/// A process sleep races its segment's cancel promise, and the race's winner
/// is a journal fact (FIG-3673): the live drive that lost its sleep to the
/// promise settles cancelled, and a redrive against state that no longer
/// carries the cancellation settles cancelled too, from the journal alone.
#[tokio::test]
pub(super) async fn a_process_sleep_that_lost_to_the_cancel_promise_replays_cancelled() {
    let process_id = ProcessId::from("sleep-cancel-race-replay");
    let registration = sleeping_process_registration(&process_id).await;
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        &process_id,
        "sleep-cancel-race-replay-invocation",
    );
    let live = {
        let context = Arc::clone(&context);
        let registration = registration.clone();
        let process_id = process_id.clone();
        let execution_write_authority = execution_write_authority.clone();
        tokio::spawn(async move {
            drive_sleeping_process(
                &process_id,
                &registration,
                &context,
                &execution_write_authority,
            )
            .await
        })
    };

    context.await_sleep_started().await;
    // Only the durable promise resolves; the timer stays parked.
    context.commit_process_cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), live)
        .await
        .expect("the promise ends the parked sleep")
        .expect("join live sleeping process")
        .expect("run live sleeping process");
    assert!(
        is_cancelled_terminal(&outcome),
        "the promise that won the race must settle the live run cancelled: {outcome:#?}"
    );
    assert_eq!(context.process_cancel_race_verdicts(), vec![true]);

    // Redelivery lands against state that no longer carries the cancellation:
    // only the journal still holds the race the live drive recorded.
    context.clear_process_cancel();
    context.start_replay();
    let replayed = tokio::time::timeout(
        Duration::from_secs(10),
        drive_sleeping_process(
            &process_id,
            &registration,
            &context,
            &execution_write_authority,
        ),
    )
    .await
    .expect("the replayed race answers from the journal")
    .expect("replay the recorded race");
    assert!(
        is_cancelled_terminal(&replayed),
        "the replayed sleep must read the recorded winner, not live state: {replayed:#?}"
    );
    assert_eq!(
        context.process_cancel_race_verdicts(),
        vec![true],
        "a replay consumes the recorded race instead of recording a new one"
    );
}

/// The converse: a cancel that commits after the timer won is not read back
/// into the replayed wake (FIG-3673). The recorded race says the timer won, so
/// the replay reaches the same settled terminal the live drive did.
#[tokio::test]
pub(super) async fn a_cancel_committed_after_the_timer_won_does_not_rewrite_the_replayed_wake() {
    let process_id = ProcessId::from("sleep-cancel-race-late");
    let registration = sleeping_process_registration(&process_id).await;
    let context = Arc::new(ReplayableRecordingContext::default());
    let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        &process_id,
        "sleep-cancel-race-late-invocation",
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        drive_sleeping_process(
            &process_id,
            &registration,
            &context,
            &execution_write_authority,
        ),
    )
    .await
    .expect("the unparked timer completes")
    .expect("run live sleeping process");
    assert!(
        !is_cancelled_terminal(&outcome),
        "no cancel was committed while the timer raced: {outcome:#?}"
    );
    assert_eq!(context.process_cancel_race_verdicts(), vec![false]);

    context.commit_process_cancel();
    context.start_replay();
    let replayed = tokio::time::timeout(
        Duration::from_secs(10),
        drive_sleeping_process(
            &process_id,
            &registration,
            &context,
            &execution_write_authority,
        ),
    )
    .await
    .expect("the replayed race answers from the journal")
    .expect("replay the recorded race");
    assert_eq!(
        replayed, outcome,
        "the replay must reproduce the live terminal, not the later cancel"
    );
}
