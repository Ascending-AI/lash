use super::*;

// FIG-3149: the wake verdict for a durable process sleep is journaled, not
// re-read live. A redrive of the same wake must observe exactly the verdict the
// live wake committed, even when live cancellation state has since changed.
#[tokio::test]
pub(super) async fn process_sleep_wake_verdict_replays_from_the_journal() {
    let process_id = ProcessId::from("sleep-cancel-verdict-replay");
    let (registry, continuations) = process_stores();
    let registration = sleeping_process_registration(&process_id).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register sleeping process");
    let worker = recovery_worker(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(worker)),
        Arc::clone(&registry),
        continuations,
    ));
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        &process_id,
        "sleep-cancel-verdict-replay-invocation",
    );
    let live = {
        let workflow = Arc::clone(&workflow);
        let context = Arc::clone(&context);
        let registration = registration.clone();
        let process_id = process_id.clone();
        let execution_write_authority = execution_write_authority.clone();
        tokio::spawn(async move {
            let controller = RestateRuntimeEffectController::new_for_test(context);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_write_authority),
                    controller
                        .scoped_effect_controller(durable_admission(&ExecutionScope::process(
                            &process_id,
                        )))
                        .expect("verdict replay scope"),
                    0,
                    None,
                    pending_process_cancel_signal(),
                )
                .await
        })
    };

    context.await_sleep_started().await;
    // Only the durable promise is resolved here: the live cancellation watch
    // stays pending, so the wake verdict is the sole cancellation path.
    context.commit_process_cancel();
    context.release_sleep();

    let outcome = live
        .await
        .expect("join live sleeping process")
        .expect("run live sleeping process");
    assert!(
        matches!(
            outcome,
            lash_core::ProcessRunOutcome::Terminal { ref output, .. }
                if output.terminal_status() == Some(lash_core::ProcessStatus::Cancelled)
        ),
        "the journaled wake verdict must settle the live run cancelled: {outcome:#?}"
    );
    assert_eq!(
        context.process_cancel_wake_verdicts(),
        vec![true],
        "the live wake must record exactly one verdict"
    );

    // Redelivery lands against durable state that no longer carries the
    // cancellation: only the journal still holds the verdict the live wake
    // committed.
    let (replay_registry, replay_continuations) = process_stores();
    replay_registry
        .register_process(registration.clone())
        .await
        .expect("register the redelivered process");
    let replay_worker = recovery_worker(
        Arc::clone(&replay_registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let replay_workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(replay_worker)),
        Arc::clone(&replay_registry),
        replay_continuations,
    ));
    context.clear_process_cancel();
    context.start_replay();
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let replayed = replay_workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default()
                .with_execution_write_authority(execution_write_authority),
            controller
                .scoped_effect_controller(durable_admission(&ExecutionScope::process(
                    &process_id,
                )))
                .expect("verdict replay redelivery scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("replay the journaled wake");
    assert!(
        matches!(
            replayed,
            lash_core::ProcessRunOutcome::Terminal { ref output, .. }
                if output.terminal_status() == Some(lash_core::ProcessStatus::Cancelled)
        ),
        "the replayed wake must read the journaled verdict, not live state: {replayed:#?}"
    );
    assert_eq!(
        context.process_cancel_wake_verdicts(),
        vec![true],
        "the replayed wake must consume the recorded verdict instead of recording a new one"
    );
}

// Deployment compatibility gate, FIG-790 style: a process invocation journaled
// before the wake verdict command existed carries no verdict entry. Its redrive
// must accept that prefix and append the verdict rather than diverge.
#[tokio::test]
pub(super) async fn process_sleep_wake_verdict_extends_a_pre_verdict_journal() {
    let process_id = ProcessId::from("sleep-cancel-verdict-compat");
    let (registry, continuations) = process_stores();
    let registration = sleeping_process_registration(&process_id).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register sleeping process");
    let worker = recovery_worker(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(worker)),
        Arc::clone(&registry),
        continuations,
    ));
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        &process_id,
        "sleep-cancel-verdict-compat-invocation",
    );
    let deployed = {
        let workflow = Arc::clone(&workflow);
        let context = Arc::clone(&context);
        let registration = registration.clone();
        let process_id = process_id.clone();
        let execution_write_authority = execution_write_authority.clone();
        tokio::spawn(async move {
            let controller = RestateRuntimeEffectController::new_for_test(context);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_write_authority),
                    controller
                        .scoped_effect_controller(durable_admission(&ExecutionScope::process(
                            &process_id,
                        )))
                        .expect("pre-verdict journal scope"),
                    0,
                    None,
                    pending_process_cancel_signal(),
                )
                .await
        })
    };

    context.await_sleep_started().await;
    context.release_sleep();
    deployed
        .await
        .expect("join pre-verdict process")
        .expect("run pre-verdict process");

    // Drop the verdict entry: this is the journal shape a deployment written
    // before FIG-3149 left behind.
    context.forget_process_cancel_wake_verdicts();
    context.commit_process_cancel();
    context.start_replay_allowing_journal_extension();

    let (replay_registry, replay_continuations) = process_stores();
    replay_registry
        .register_process(registration.clone())
        .await
        .expect("register the redelivered process");
    let replay_worker = recovery_worker(
        Arc::clone(&replay_registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let replay_workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(replay_worker)),
        Arc::clone(&replay_registry),
        replay_continuations,
    ));
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let redriven = replay_workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default()
                .with_execution_write_authority(execution_write_authority),
            controller
                .scoped_effect_controller(durable_admission(&ExecutionScope::process(
                    &process_id,
                )))
                .expect("pre-verdict journal redelivery scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("new code must redrive a journal written before the wake verdict existed");
    assert!(
        matches!(redriven, lash_core::ProcessRunOutcome::Terminal { .. }),
        "the pre-verdict journal must redrive to a terminal: {redriven:#?}"
    );
    assert_eq!(
        context.process_cancel_wake_verdicts(),
        vec![true],
        "the redrive must append exactly one wake verdict to the deployed prefix"
    );
}
