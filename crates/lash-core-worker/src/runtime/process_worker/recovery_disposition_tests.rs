use super::*;

#[tokio::test]
async fn recovered_nested_registry_read_uses_backend_error_telemetry() {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let fail = Arc::new(tokio::sync::Notify::new());
    let (worker, registry, _, env_ref, test_registry) = worker_with_engine_and_registry(
        1,
        Arc::new(PausedInfraEngine { started, fail }),
        run_handle,
    )
    .await;
    let _process_id = "nested-recovery-read-failure";
    let paused_infra_record = registry
        .register_process(engine_registration(
            "paused-infra",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register process");
    let process_id = paused_infra_record.id.clone();
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("process exists");
    test_registry.set_process_read_error_after(
        1,
        PluginError::Session("injected nested registry read failure".to_string()),
    );

    let (_outcome, capture) = capturing(|| worker.recover_process(record)).await;

    assert_recovery_backend_error_event(
        &capture,
        &process_id,
        "read_process",
        "plugin session error: injected nested registry read failure",
    );
}

#[tokio::test]
async fn recovered_live_renewal_uses_backend_error_telemetry() {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let fail = Arc::new(tokio::sync::Notify::new());
    let timings = crate::LeaseTimings::new(Duration::from_millis(30), Duration::from_millis(10))
        .expect("valid short lease timings");
    let (worker, registry, _, env_ref, test_registry) = worker_with_engine_registry_and_timings(
        1,
        Arc::new(PausedInfraEngine {
            started: Arc::clone(&started),
            fail,
        }),
        run_handle,
        Some(timings),
    )
    .await;
    let _process_id = "live-recovery-renewal-failure";
    let paused_infra_record = registry
        .register_process(engine_registration(
            "paused-infra",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register process");
    let process_id = paused_infra_record.id.clone();
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("process exists");
    test_registry.set_process_lease_renew_error(Some(PluginError::Session(
        "injected live lease-renewal failure".to_string(),
    )));

    let (_outcome, capture) = capturing(|| worker.recover_process(record)).await;

    assert_recovery_backend_error_event(
        &capture,
        &process_id,
        "renew_lease",
        "plugin session error: injected live lease-renewal failure",
    );
    assert!(
        registry
            .get_process_lease(&process_id)
            .await
            .expect("read lease")
            .is_none(),
        "token-fenced release makes the backend-error row immediately claimable"
    );
}
