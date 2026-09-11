use super::*;

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_retry_keeps_the_admitted_deployment_configuration() {
    run_async_test_on_stack_budget_multi_thread("workbench-immutable-deployment-retry", 4, || {
        live_restate_retry_keeps_the_admitted_deployment_configuration_inner()
    });
}

async fn live_restate_retry_keeps_the_admitted_deployment_configuration_inner() {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url =
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let mutate_reused_endpoint =
        std::env::var_os("AGENT_WORKBENCH_E2E_MUTATE_REUSED_ENDPOINT").is_some();

    let a_dir = tempfile::tempdir().expect("create fixture A data directory");
    let a_path = a_dir.path().to_path_buf();
    let a_sessions = WorkbenchSessions::persistent(a_path.join("session-id"))
        .expect("open fixture A persistent session selection");
    let a_active_turns = ActiveTurns::persistent(a_path.join("active-turns.json"))
        .expect("open fixture A active-turn routing");
    let a_lease_timings = recovery_e2e_lease_timings();
    let a_provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (a_retry_entered_tx, mut a_retry_entered_rx) = mpsc::unbounded_channel();
    let a_provider =
        immutable_deployment_fixture_a_provider(Arc::clone(&a_provider_calls), a_retry_entered_tx);
    let harness_a = live_workbench_restate_state_with_provider_and_database(
        &a_path,
        ingress_url.clone(),
        a_provider.clone(),
        a_sessions,
        a_active_turns,
        None,
        a_lease_timings,
    )
    .await;
    let mut endpoint_a = LiveRestateEndpoint::start(
        &admin_url,
        harness_a.state.clone(),
        harness_a.process_deployment,
        harness_a.process_worker,
    )
    .await;
    let endpoint_a_addr = endpoint_a.addr;
    let deployment_a = endpoint_a.deployment_id.clone();
    println!(
        "immutable deployment fixture A: uri={} deployment={} data={}",
        endpoint_a.endpoint_url,
        deployment_a,
        a_path.display()
    );

    let (invocation_a, _) = submit_workbench_turn_via_restate(
        &harness_a.state,
        "journal fixture A process registration before a transport retry",
    )
    .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(20), a_retry_entered_rx.recv())
            .await
            .expect("fixture A post-registration provider call timeout"),
        Some(()),
        "fixture A must reach the provider gate after journaling its process registration"
    );
    let process_id = wait_for_running_process(
        &harness_a.state,
        "immutable_deployment_probe",
        Duration::from_secs(20),
    )
    .await;
    let process_before = harness_a
        .state
        .process_observer
        .process(&process_id)
        .await
        .expect("read fixture A process before retry")
        .expect("fixture A process before retry");
    let env_ref_before = process_before
        .env_ref
        .clone()
        .expect("fixture A process execution environment reference");
    let env_bytes_before = harness_a
        .process_env_store
        .get_process_execution_env(&env_ref_before)
        .await
        .expect("read fixture A process execution environment")
        .expect("fixture A process execution environment bytes");
    let admitted_a = wait_for_invocation_deployment(
        &admin_url,
        &invocation_a,
        &deployment_a,
        Duration::from_secs(20),
    )
    .await;
    assert!(
        matches!(admitted_a.status.as_str(), "running" | "suspended"),
        "fixture A must be executing when its journal prefix is captured: {admitted_a:#?}"
    );
    let lash_drain = harness_a
        .state
        .core
        .drain_status(false)
        .await
        .expect("query fixture A drain precondition");
    assert!(
        !lash_drain.drained && lash_drain.remaining_invocations > 0,
        "fixture A must retain its journaled process before transport interruption: {lash_drain:#?}"
    );
    let a_session_id = harness_a.state.current_session_id();
    let lease_generation_before = session_lease_generation(&a_path, "sqlite", &a_session_id).await;

    endpoint_a.stop().await;
    // The listener/runtime is gone, but this outer test handle still owns the
    // original storage clients. Retire those clients before rebuilding A so
    // the restart models one host generation rather than two live owners of
    // the same SQLite session store.
    drop(harness_a.state);
    drop(harness_a.process_env_store);
    drop(harness_a.trace_path);
    let interrupted_a = wait_for_invocation_deployment(
        &admin_url,
        &invocation_a,
        &deployment_a,
        Duration::from_secs(20),
    )
    .await;
    assert!(
        interrupted_a.is_still_active(),
        "stopping fixture A must leave its exact invocation replayable: {interrupted_a:#?}"
    );

    let b_dir = tempfile::tempdir().expect("create fixture B data directory");
    let b_path = b_dir.path().to_path_buf();
    let b_provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let b_provider_calls_for_provider = Arc::clone(&b_provider_calls);
    let provider_b = lash::testing::TestProvider::builder()
        .kind("workbench-immutable-deployment-fixture-b")
        .complete(move |_| {
            let calls = Arc::clone(&b_provider_calls_for_provider);
            async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(text_response(
                    "<lashlang>\nfinish \"fixture B completed\"\n</lashlang>",
                ))
            }
        })
        .build()
        .into_handle();
    let harness_b = live_workbench_restate_state_with_provider(
        &b_path,
        ingress_url.clone(),
        provider_b,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    let mut endpoint_b = if mutate_reused_endpoint {
        LiveRestateEndpoint::start_replacing_for_mutation(
            &admin_url,
            endpoint_a_addr,
            harness_b.state.clone(),
            harness_b.process_deployment,
            harness_b.process_worker,
        )
        .await
    } else {
        LiveRestateEndpoint::start(
            &admin_url,
            harness_b.state.clone(),
            harness_b.process_deployment,
            harness_b.process_worker,
        )
        .await
    };
    let deployment_b = endpoint_b.deployment_id.clone();
    println!(
        "immutable deployment fixture B: uri={} deployment={} data={}",
        endpoint_b.endpoint_url,
        deployment_b,
        b_path.display()
    );
    if !mutate_reused_endpoint {
        assert_ne!(
            deployment_b, deployment_a,
            "distinct fixture configurations require distinct Restate deployments"
        );
    }
    let invocation_b =
        run_workbench_turn_via_restate(&harness_b.state, "route new work to fixture B").await;
    wait_for_restate_invocation_success(&harness_b.state, &invocation_b, Duration::from_secs(20))
        .await;
    wait_for_workbench_message(
        &harness_b.state,
        "fixture B completed",
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        b_provider_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "fixture B must receive exactly its newly admitted invocation"
    );
    let admitted_b = wait_for_invocation_deployment(
        &admin_url,
        &invocation_b,
        &deployment_b,
        Duration::from_secs(20),
    )
    .await;
    assert!(admitted_b.completed_successfully());

    let a_sessions = WorkbenchSessions::persistent(a_path.join("session-id"))
        .expect("reopen fixture A persistent session selection");
    let a_active_turns = ActiveTurns::persistent(a_path.join("active-turns.json"))
        .expect("reopen fixture A active-turn routing");
    let a_restart_lease_timings = recovery_e2e_lease_timings();
    assert_eq!(
        a_restart_lease_timings, a_lease_timings,
        "fixture A restart must retain its original lease configuration"
    );
    let harness_a = live_workbench_restate_state_with_provider_and_database(
        &a_path,
        ingress_url,
        a_provider,
        a_sessions,
        a_active_turns,
        None,
        a_restart_lease_timings,
    )
    .await;
    if mutate_reused_endpoint {
        wait_for_restate_invocation_success_admin(
            &admin_url,
            &invocation_a,
            Duration::from_secs(12),
            "mutable endpoint replacement changed fixture A's admitted configuration",
        )
        .await;
    } else {
        endpoint_a = LiveRestateEndpoint::restart(
            endpoint_a_addr,
            harness_a.state.clone(),
            harness_a.process_deployment,
            harness_a.process_worker,
            deployment_a.clone(),
        )
        .await;
        wait_for_restate_invocation_success(
            &harness_a.state,
            &invocation_a,
            Duration::from_secs(30),
        )
        .await;
    }
    assert!(
        a_provider_calls.load(std::sync::atomic::Ordering::SeqCst) >= 3,
        "fixture A's admitted invocation must retry through fixture A's provider"
    );
    assert_eq!(
        b_provider_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "fixture A's admitted invocation must not reach fixture B's provider"
    );
    wait_for_workbench_message(
        &harness_a.state,
        "fixture A completed",
        Duration::from_secs(20),
    )
    .await;
    let completed_a = wait_for_invocation_deployment(
        &admin_url,
        &invocation_a,
        &deployment_a,
        Duration::from_secs(20),
    )
    .await;
    assert!(completed_a.completed_successfully());
    assert!(
        session_lease_generation(&a_path, "sqlite", &a_session_id).await > lease_generation_before,
        "fixture A retry must supersede the interrupted host's session-lease generation"
    );
    let process_after = harness_a
        .state
        .process_observer
        .process(&process_id)
        .await
        .expect("read fixture A process after retry")
        .expect("fixture A process after retry");
    assert_eq!(
        process_after.env_ref.as_ref(),
        Some(&env_ref_before),
        "fixture A retry reconstructed a different process environment reference"
    );
    let env_bytes_after = harness_a
        .process_env_store
        .get_process_execution_env(&env_ref_before)
        .await
        .expect("read fixture A process environment after retry")
        .expect("fixture A process environment bytes after retry");
    assert_eq!(
        env_bytes_after, env_bytes_before,
        "fixture A retry reconstructed different process environment bytes"
    );
    endpoint_a
        .stop_after_producers_closed_and_drained(&harness_a.state, Duration::from_secs(30))
        .await;
    endpoint_b
        .stop_after_producers_closed_and_drained(&harness_b.state, Duration::from_secs(30))
        .await;
    let a_path_for_check = a_path.clone();
    let b_path_for_check = b_path.clone();
    a_dir.close().expect("remove fixture A data directory");
    b_dir.close().expect("remove fixture B data directory");
    assert!(!a_path_for_check.exists());
    assert!(!b_path_for_check.exists());
    println!(
        "workbench immutable-deployment gate passed: A-retried-on-A; B-new-on-B; env-ref-and-bytes-stable; owned-cleanup"
    );
}

fn immutable_deployment_fixture_a_provider(
    calls: Arc<std::sync::atomic::AtomicUsize>,
    retry_entered: mpsc::UnboundedSender<()>,
) -> ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("workbench-immutable-deployment-fixture-a")
        .complete(move |request| {
            let calls = Arc::clone(&calls);
            let retry_entered = retry_entered.clone();
            async move {
                assert!(
                    request.stream_events.is_some(),
                    "fixture A process body must not call the provider"
                );
                match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => Ok(text_response(
                        r#"<lashlang>
process immutable_deployment_probe() {
  sleep for "15s"
  finish "fixture A process completed"
}
handle = start immutable_deployment_probe()
"fixture A journal prefix committed"
</lashlang>"#,
                    )),
                    1 => {
                        let _ = retry_entered.send(());
                        std::future::pending().await
                    }
                    _ => Ok(text_response(
                        "<lashlang>\nfinish \"fixture A completed\"\n</lashlang>",
                    )),
                }
            }
        })
        .build()
        .into_handle()
}

async fn wait_for_invocation_deployment(
    admin_url: &str,
    invocation_id: &lash_restate::RestateInvocationId,
    deployment_id: &str,
    timeout: Duration,
) -> lash_restate::RestateInvocationStatus {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let status = restate_invocation_status_with_deployment(admin_url, invocation_id).await;
        if let Some(ref status) = status
            && status.pinned_deployment_id.as_deref() == Some(deployment_id)
        {
            return status.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "invocation {invocation_id} was not pinned to deployment {deployment_id} within {timeout:?}; last status={status:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_restate_invocation_success_admin(
    admin_url: &str,
    invocation_id: &lash_restate::RestateInvocationId,
    timeout: Duration,
    failure: &str,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::new(admin_url));
    loop {
        let status = admin
            .invocation_status(invocation_id)
            .await
            .expect("query Restate invocation after mutable replacement");
        if status
            .as_ref()
            .is_some_and(|status| status.completed_successfully())
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{failure}: invocation {invocation_id} did not complete under its original configuration; last status={status:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
