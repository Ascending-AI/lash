use super::*;

#[tokio::test]
async fn restate_replay_does_not_reexecute_process_owned_tool_call() {
    let process_id = "restate-process-tool-replay";
    let executions = Arc::new(AtomicUsize::new(0));
    let registry = process_registry();
    let store_factory: Arc<dyn lash_core::SessionStoreFactory> =
        memory_session_store_factory().await;
    let env_ref = persist_recovery_env_ref().await;
    let registration =
        counting_tool_registration(process_id, lash_core::RecoveryContract::Rerunnable, env_ref);
    registry
        .register_process(registration.clone())
        .await
        .expect("register replay process");
    let worker = recovery_worker_with_plugins(
        Arc::clone(&registry),
        store_factory,
        vec![counting_tool_plugin(Arc::clone(&executions))],
    )
    .await;
    let context = Arc::new(ReplayableRecordingContext::default());

    let first_controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let first_scope = first_controller
        .process_scope_for_test(
            recorded_process_admission(registry.as_ref(), &ProcessId::from(process_id)).await,
        )
        .expect("scope first process execution");
    let first = worker
        .run_process_segment_with_scoped_effect_controller(
            registration.clone(),
            ProcessExecutionContext::default(),
            lash_core::ProcessExecutionWriteAuthority::invocation(
                process_id,
                "process-tool-replay-execution",
            ),
            first_scope,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect("run first process execution");
    assert!(matches!(
        first,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if matches!(*output, ProcessAwaitOutput::Settled { ref output } if output.is_success())
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    context.start_replay();
    let replay_controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let replay_scope = replay_controller
        .process_scope_for_test(
            recorded_process_admission(registry.as_ref(), &ProcessId::from(process_id)).await,
        )
        .expect("scope replayed process execution");
    let replayed = worker
        .run_process_segment_with_scoped_effect_controller(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ProcessExecutionWriteAuthority::invocation(
                process_id,
                "process-tool-replay-execution",
            ),
            replay_scope,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect("replay process execution");

    assert!(matches!(
        replayed,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if matches!(*output, ProcessAwaitOutput::Settled { ref output } if output.is_success())
    ));
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "Restate replay must return the journaled process ToolAttempt instead of re-executing the provider"
    );
}

/// A redrive that reaches the runner after the segment's completion step
/// stored its terminal replays the runner over its journal (FIG-3673). The
/// segment's start was recorded by its admission, so the worker reads it and
/// never runs the start CAS again: that live write would refuse a terminal
/// process on every such redrive and turn the recorded journal into a
/// terminal failure of the attempt.
#[tokio::test]
async fn a_redrive_after_the_terminal_is_stored_replays_the_runner() {
    let process_id = "restate-process-redrive-after-complete";
    let executions = Arc::new(AtomicUsize::new(0));
    let registry = process_registry();
    let store_factory: Arc<dyn lash_core::SessionStoreFactory> =
        memory_session_store_factory().await;
    let env_ref = persist_recovery_env_ref().await;
    let registration =
        counting_tool_registration(process_id, lash_core::RecoveryContract::Rerunnable, env_ref);
    registry
        .register_process(registration.clone())
        .await
        .expect("register redriven process");
    let worker = recovery_worker_with_plugins(
        Arc::clone(&registry),
        store_factory,
        vec![counting_tool_plugin(Arc::clone(&executions))],
    )
    .await;
    let context = Arc::new(ReplayableRecordingContext::default());
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id,
        "process-redrive-after-complete-execution",
    );

    let first_controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let first = worker
        .run_process_segment_with_scoped_effect_controller(
            registration.clone(),
            ProcessExecutionContext::default(),
            authority.clone(),
            first_controller
                .process_scope_for_test(
                    recorded_process_admission(registry.as_ref(), &ProcessId::from(process_id))
                        .await,
                )
                .expect("scope first process execution"),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect("run first process execution");
    let lash_core::ProcessRunOutcome::Terminal { output, .. } = &first else {
        panic!("the first execution reaches its terminal: {first:?}");
    };
    // The completion step's body: the terminal is stored before the attempt
    // that proposed it is acknowledged.
    registry
        .complete_process(
            &ProcessId::from(process_id),
            (**output).clone(),
            workflow_key_authority(&ProcessId::from(process_id)),
        )
        .await
        .expect("store the terminal");

    context.start_replay();
    let replay_controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let replayed = worker
        .run_process_segment_with_scoped_effect_controller(
            registration,
            ProcessExecutionContext::default(),
            authority,
            replay_controller
                .process_scope_for_test(
                    recorded_process_admission(registry.as_ref(), &ProcessId::from(process_id))
                        .await,
                )
                .expect("scope redriven process execution"),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect("a redrive over a stored terminal replays the runner");
    assert_eq!(
        replayed, first,
        "the redrive reproduces the recorded terminal"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

async fn signal_waiting_process_registration(process_id: &ProcessId) -> ProcessRegistration {
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let linked = lash_typescript::link(
        r#"
        const worker = async () => {
          await waitSignal("go");
          return "signalled";
        };
        finish(null);
        "#,
        &environment,
    )
    .expect("link the signal-waiting TypeScript process");
    lashlang::LashlangArtifactStore::publish_module_artifact(
        recovery_artifact_store().as_ref(),
        &lash_core::ArtifactOwner::host("restate-recovery-test"),
        &linked.artifact,
    )
    .await
    .expect("store the signal-waiting process artifact");
    let worker = sole_lifted_process_name(&linked.artifact);
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref(&worker)
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name: worker,
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

/// A body whose final segment ran `waitSignal`, redriven after the
/// completion step stored its terminal, replays its wait-state writes as the
/// steps it recorded: the registry, which refuses a terminal process's wait
/// state, is not asked again, so the body reissues the wait it recorded and
/// reaches the same terminal (FIG-3673).
#[tokio::test]
async fn a_wait_signal_body_redriven_over_its_stored_terminal_replays_its_wait_steps() {
    let process_id = ProcessId::from("restate-process-wait-signal-redrive");
    let registry = process_registry();
    let store_factory: Arc<dyn lash_core::SessionStoreFactory> =
        memory_session_store_factory().await;
    let registration = signal_waiting_process_registration(&process_id).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register the signal-waiting process");
    let worker = recovery_worker(Arc::clone(&registry), store_factory).await;
    let context = Arc::new(ReplayableRecordingContext::default());
    let signal_key = restate_await_event_key_for_authority(
        &test_restate_authority_id(),
        &ExecutionScope::process(&process_id),
        AwaitEventWaitIdentity::process_signal(&process_id, "go", 1),
    )
    .expect("the signal wait's key");
    context
        .events
        .resolve_durable_event(RestateDurableWaitResolveRequest {
            key: signal_key,
            resolution: Resolution::Ok(serde_json::json!({ "go": true })),
        });
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        &process_id,
        "process-wait-signal-redrive-execution",
    );
    let run = |context: Arc<ReplayableRecordingContext>| {
        let worker = worker.clone();
        let registry = Arc::clone(&registry);
        let registration = registration.clone();
        let authority = authority.clone();
        let process_id = process_id.clone();
        async move {
            let controller = RestateRuntimeEffectController::with_options_for_test(
                context,
                RestateEffectControllerOptions::default().process_segment_drive(),
            );
            worker
                .run_process_segment_with_scoped_effect_controller(
                    registration,
                    ProcessExecutionContext::default(),
                    authority,
                    controller
                        .process_scope_for_test(
                            recorded_process_admission(registry.as_ref(), &process_id).await,
                        )
                        .expect("scope the signal-waiting process"),
                    tokio_util::sync::CancellationToken::new(),
                    None,
                )
                .await
        }
    };

    let first = run(Arc::clone(&context))
        .await
        .expect("run the signal-waiting process");
    let lash_core::ProcessRunOutcome::Terminal { output, .. } = &first else {
        panic!("the first execution reaches its terminal: {first:?}");
    };
    assert!(is_process_success(output), "the signal arrived: {output:?}");
    let steps = context.runs();
    assert!(
        steps
            .iter()
            .any(|name| name.starts_with("lash.process.wait.enter:")),
        "entering the wait is a recorded step: {steps:?}"
    );
    assert!(
        steps
            .iter()
            .any(|name| name.starts_with("lash.process.wait.clear:")),
        "clearing the wait is a recorded step: {steps:?}"
    );
    // The completion step's body: the terminal is stored before the attempt
    // that proposed it is acknowledged.
    registry
        .complete_process(
            &process_id,
            (**output).clone(),
            workflow_key_authority(&process_id),
        )
        .await
        .expect("store the terminal");

    context.start_replay();
    let replayed = run(Arc::clone(&context))
        .await
        .expect("a redrive over the stored terminal replays its wait");
    assert_eq!(
        replayed, first,
        "the redrive reproduces the recorded terminal"
    );
}
