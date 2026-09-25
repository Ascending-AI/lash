use super::*;

/// FIG-3424 Restate leg: the SessionTurn run owns its child runtime. Once the
/// workflow invocation returns, the `Weak` captured at session initialisation
/// no longer upgrades — nothing caches the child — while the durable child
/// row remains and reopens through the ordinary store open.
#[tokio::test]
pub(super) async fn session_turn_child_runtime_does_not_outlive_the_process_run() {
    let process_id = ProcessId::from("session-turn-child-liveness");
    let (registry, continuations) = process_stores();
    let registration = rerunnable_session_turn_registration("session-turn-child-liveness");
    registry
        .register_process(registration.clone())
        .await
        .expect("register session turn");

    // A worker like `recovery_worker`, with a scripted provider so the child
    // turn completes instead of failing on provider resolution.
    let watched = lash_core::facade_support::watch_process_registry(Arc::clone(&registry));
    let plugin_host = lash_core::facade_support::PluginHost::new(vec![Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    )
        as Arc<dyn lash_core::facade_support::PluginFactory>]);
    let mut runtime_host = memory_host_config().await;
    // The run executes under the journaled Restate controller, so the
    // worker's effect host must present the same turn-control authority.
    runtime_host.control.effect_host = Arc::new(RestateEffectHost::new(
        RestateConnection::new("https://restate.invalid"),
        test_restate_authority_id(),
    ));
    runtime_host.providers.provider_resolver =
        Arc::new(lash_core::facade_support::SingleProviderResolver::new(
            lash_core::testing::runtime_helpers::mock_provider(vec![
                lash_core::testing::runtime_helpers::MockCall {
                    stream_events: Vec::new(),
                    response: Ok(lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "child answered".to_string(),
                            response_meta: None,
                        }],
                        ..Default::default()
                    }),
                },
            ])
            .into_handle(),
        ));
    let session_store_factory = runtime_host.session_store_factory();
    let worker = DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            lash_core_worker::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(lash_core::SessionPolicy {
            provider_id: "mock".to_string(),
            ..recovery_session_policy()
        }),
    )
    .expect("valid liveness worker");
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(worker)),
        Arc::clone(&registry),
        continuations,
    ));

    let context = Arc::new(ReplayableRecordingContext::default());
    let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        &process_id,
        "session-turn-liveness-invocation",
    );
    let _ = lash_core::runtime::take_spawned_child_runtimes();
    let outcome = workflow
        .run_registration_for_test(
            registration,
            ProcessExecutionContext::default()
                .with_execution_write_authority(execution_write_authority),
            RestateRuntimeEffectController::new_for_test(context)
                .process_scope_for_test(durable_admission(&ExecutionScope::process(&process_id)))
                .expect("session-turn liveness process scope"),
            0,
            None,
        )
        .await
        .expect("session turn run");
    assert!(
        matches!(
            outcome,
            lash_core::ProcessRunOutcome::Terminal { ref output, .. }
                if output.terminal_status() == Some(lash_core::ProcessStatus::Completed)
        ),
        "the child turn completed: {outcome:#?}"
    );

    let spawned = lash_core::runtime::take_spawned_child_runtimes();
    assert_eq!(spawned.len(), 1, "the run minted exactly one child runtime");
    let child_session_id = spawned[0].0.clone();
    assert!(
        spawned.iter().all(|(_, weak)| weak.upgrade().is_none()),
        "the child runtime must be dropped when the Restate process run ends"
    );
    assert!(
        session_store_factory
            .open_existing_store_by_id(&child_session_id)
            .await
            .expect("inspect durable child")
            .is_some(),
        "the durable child row remains and reopens through the ordinary open"
    );
}
