use super::*;

#[tokio::test]
async fn restate_replay_does_not_reexecute_process_owned_tool_call() {
    let process_id = "restate-process-tool-replay";
    let executions = Arc::new(AtomicUsize::new(0));
    let registry = process_registry();
    let store_factory: Arc<dyn lash_core::SessionStoreFactory> =
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
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
    );
    let context = Arc::new(ReplayableRecordingContext::default());

    let first_controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let first_scope = first_controller
        .scoped_effect_controller(ExecutionScope::process(process_id))
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
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Success { .. })
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    context.start_replay();
    let replay_controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let replay_scope = replay_controller
        .scoped_effect_controller(ExecutionScope::process(process_id))
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
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Success { .. })
    ));
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "Restate replay must return the journaled process ToolAttempt instead of re-executing the provider"
    );
}
