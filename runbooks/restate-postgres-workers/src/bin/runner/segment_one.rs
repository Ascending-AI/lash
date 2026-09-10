use super::*;

pub(super) struct SegmentOneOutput {
    pub(super) trigger_process_id: ProcessId,
    pub(super) signal_process_id: ProcessId,
}

pub(super) async fn run_workflow_segment_one(
    storage: &PostgresStorage,
    admin_url: &str,
    ingress_url: &str,
    mock_provider_base_url: &str,
    trace_dir: Option<PathBuf>,
) -> Result<SegmentOneOutput> {
    run_cold_process_await_event_vectors(admin_url, ingress_url).await?;

    let main_request = TurnRequest {
        workflow_id: "e2e-main".to_string(),
        fail_once: false,
        scenario: TurnScenario::KitchenSink,
        signal: None,
    };
    submit_workflow(ingress_url, &main_request).await?;
    let main_response = wait_for_terminal_result(storage.pool(), &main_request.workflow_id).await?;
    assert_kitchen_sink_response(&main_response, true)?;
    wait_for_queued_work(
        storage,
        mock_provider_base_url,
        trace_dir.clone(),
        ingress_url,
    )
    .await?;
    let main_wake_request = TurnRequest {
        workflow_id: "e2e-main-wake".to_string(),
        fail_once: false,
        scenario: TurnScenario::DrainQueued,
        signal: None,
    };
    submit_workflow(ingress_url, &main_wake_request).await?;
    let main_wake_response =
        wait_for_terminal_result(storage.pool(), &main_wake_request.workflow_id).await?;
    assert_queued_wake_response(&main_wake_response)?;

    let trigger_request = TurnRequest {
        workflow_id: "e2e-trigger-setup".to_string(),
        fail_once: false,
        scenario: TurnScenario::TriggerSetup,
        signal: None,
    };
    submit_workflow(ingress_url, &trigger_request).await?;
    let trigger_setup_response =
        wait_for_terminal_result(storage.pool(), &trigger_request.workflow_id).await?;
    assert_trigger_setup_response(&trigger_setup_response)?;
    let trigger_process_id = emit_button_event(
        storage,
        mock_provider_base_url,
        trace_dir.clone(),
        ingress_url,
    )
    .await?;
    wait_for_process_terminal(storage.pool(), &trigger_process_id).await?;

    let signal_setup_request = TurnRequest {
        workflow_id: "e2e-signal-suspend-setup".to_string(),
        fail_once: false,
        scenario: TurnScenario::SignalSuspend,
        signal: None,
    };
    submit_workflow(ingress_url, &signal_setup_request).await?;
    let signal_setup_response =
        wait_for_terminal_result(storage.pool(), &signal_setup_request.workflow_id).await?;
    let signal_process_id = assert_signal_suspend_setup_response(&signal_setup_response)?;
    wait_for_process_signal_wait(storage.pool(), &signal_process_id, "first", 1).await?;

    let failover_request = TurnRequest {
        workflow_id: "e2e-failover".to_string(),
        fail_once: true,
        scenario: TurnScenario::KitchenSink,
        signal: None,
    };
    submit_workflow(ingress_url, &failover_request).await?;
    let failover_response =
        wait_for_terminal_result(storage.pool(), &failover_request.workflow_id).await?;
    assert_kitchen_sink_response(&failover_response, true)?;
    wait_for_process_signal_wait(storage.pool(), &signal_process_id, "first", 1).await?;
    wait_for_queued_work(
        storage,
        mock_provider_base_url,
        trace_dir.clone(),
        ingress_url,
    )
    .await?;
    let failover_wake_request = TurnRequest {
        workflow_id: "e2e-failover-wake".to_string(),
        fail_once: false,
        scenario: TurnScenario::DrainQueued,
        signal: None,
    };
    submit_workflow(ingress_url, &failover_wake_request).await?;
    let failover_wake_response =
        wait_for_terminal_result(storage.pool(), &failover_wake_request.workflow_id).await?;
    assert_queued_wake_response(&failover_wake_response)?;

    submit_signal_workflow(
        ingress_url,
        storage.pool(),
        "e2e-signal-first",
        &signal_process_id,
        "first",
        "first-1",
        json!({ "phase": "first" }),
    )
    .await?;
    wait_for_process_signal_wait(storage.pool(), &signal_process_id, "second", 1).await?;
    submit_signal_workflow(
        ingress_url,
        storage.pool(),
        "e2e-signal-second",
        &signal_process_id,
        "second",
        "second-1",
        json!({ "phase": "second" }),
    )
    .await?;
    wait_for_process_terminal(storage.pool(), &signal_process_id).await?;
    assert_signal_process_output(storage.pool(), &signal_process_id).await?;

    let async_request = TurnRequest {
        workflow_id: "e2e-async-completion".to_string(),
        fail_once: false,
        scenario: TurnScenario::AsyncCompletion,
        signal: None,
    };
    submit_workflow(ingress_url, &async_request).await?;
    let async_response =
        wait_for_terminal_result(storage.pool(), &async_request.workflow_id).await?;
    assert_async_completion_response(&async_response)?;

    for (workflow_id, fail_once) in [
        ("e2e-process-llm-query", false),
        ("e2e-process-llm-query-replay", true),
    ] {
        let request = TurnRequest {
            workflow_id: workflow_id.to_string(),
            fail_once,
            scenario: TurnScenario::ProcessLlmQuery,
            signal: None,
        };
        submit_workflow(ingress_url, &request).await?;
        let response = wait_for_terminal_result(storage.pool(), workflow_id).await?;
        assert_process_llm_query_response(&response)?;
    }

    let durable_input_request = TurnRequest {
        workflow_id: "e2e-durable-input".to_string(),
        fail_once: false,
        scenario: TurnScenario::DurableInputRequest,
        signal: None,
    };
    submit_workflow(ingress_url, &durable_input_request).await?;
    let (durable_key, durable_waiter_worker) =
        wait_for_durable_input_key(storage.pool(), &durable_input_request.workflow_id).await?;
    wait_for_durable_wait_attached(admin_url, &durable_key).await?;
    let durable_resolve = resolve_durable_wait_from_peer_worker(
        &durable_waiter_worker,
        &durable_key,
        lash_core::Resolution::Ok(json!({
            "request_id": "e2e-durable-input:request-1",
            "answer": "durable-approved",
            "worker_id": "peer-worker"
        })),
    )
    .await
    .context("resolve attached durable input await key from peer worker")?;
    anyhow::ensure!(
        matches!(durable_resolve, lash_core::ResolveOutcome::Accepted),
        "durable input resolve was not accepted: {durable_resolve:?}"
    );
    let durable_response =
        wait_for_terminal_result(storage.pool(), &durable_input_request.workflow_id).await?;
    assert_durable_input_response(&durable_response)?;

    let parent_durable_input_request = TurnRequest {
        workflow_id: "e2e-parent-durable-input-after-child".to_string(),
        fail_once: false,
        scenario: TurnScenario::ParentDurableInputAfterChild,
        signal: None,
    };
    submit_workflow(ingress_url, &parent_durable_input_request).await?;
    let (parent_durable_key, parent_waiter_worker) =
        wait_for_durable_input_key(storage.pool(), &parent_durable_input_request.workflow_id)
            .await?;
    let parent_durable_resolve = resolve_durable_wait_from_peer_worker(
        &parent_waiter_worker,
        &parent_durable_key,
        lash_core::Resolution::Ok(json!({
            "request_id": "e2e-parent-durable-input-after-child:request-1",
            "answer": "parent-approved",
            "worker_id": "peer-worker"
        })),
    )
    .await
    .context("resolve parent durable input before waiter attachment from peer worker")?;
    anyhow::ensure!(
        matches!(parent_durable_resolve, lash_core::ResolveOutcome::Accepted),
        "parent durable input resolve was not accepted: {parent_durable_resolve:?}"
    );
    record_harness_signal(
        storage.pool(),
        &format!(
            "durable-input-attach:{}",
            parent_durable_input_request.workflow_id
        ),
    )
    .await?;
    let parent_durable_response =
        wait_for_terminal_result(storage.pool(), &parent_durable_input_request.workflow_id).await?;
    assert_parent_durable_input_response(&parent_durable_response)?;
    Ok(SegmentOneOutput {
        trigger_process_id,
        signal_process_id,
    })
}

pub(super) async fn run_workflow_segment_two(
    storage: &PostgresStorage,
    ingress_url: &str,
    admin_url: &str,
) -> Result<()> {
    run_engine_promise_gates(admin_url, ingress_url).await?;
    assert_no_active_lash_restate_invocations(admin_url).await?;
    assert_no_problem_lash_restate_invocations(admin_url).await?;

    let tool_batch_request = TurnRequest {
        workflow_id: "e2e-tool-batch".to_string(),
        fail_once: false,
        scenario: TurnScenario::ToolBatch,
        signal: None,
    };
    submit_workflow(ingress_url, &tool_batch_request).await?;
    let tool_batch_response =
        wait_for_terminal_result(storage.pool(), &tool_batch_request.workflow_id).await?;
    assert_tool_batch_response(&tool_batch_response)?;

    let tool_batch_failover_request = TurnRequest {
        workflow_id: "e2e-tool-batch-failover".to_string(),
        fail_once: true,
        scenario: TurnScenario::ToolBatch,
        signal: None,
    };
    submit_workflow(ingress_url, &tool_batch_failover_request).await?;
    let tool_batch_failover_response =
        wait_for_terminal_result(storage.pool(), &tool_batch_failover_request.workflow_id).await?;
    assert_tool_batch_response(&tool_batch_failover_response)?;

    let segment_loop_request = TurnRequest {
        workflow_id: "e2e-segment-loop".to_string(),
        fail_once: false,
        scenario: TurnScenario::SegmentLoop,
        signal: None,
    };
    submit_workflow(ingress_url, &segment_loop_request).await?;
    let segment_loop_response =
        wait_for_terminal_result(storage.pool(), &segment_loop_request.workflow_id).await?;
    assert_segment_loop_response(&segment_loop_response)?;

    let frame_queued_request = TurnRequest {
        workflow_id: "e2e-frame-switch-queued".to_string(),
        fail_once: false,
        scenario: TurnScenario::FrameSwitchQueued,
        signal: None,
    };
    submit_workflow(ingress_url, &frame_queued_request).await?;
    let frame_queued_response =
        wait_for_terminal_result(storage.pool(), &frame_queued_request.workflow_id).await?;
    assert_frame_switch_queued_response(&frame_queued_response)?;

    let frame_prepared_request = TurnRequest {
        workflow_id: "e2e-frame-switch-prepared".to_string(),
        fail_once: false,
        scenario: TurnScenario::FrameSwitchPrepared,
        signal: None,
    };
    submit_workflow(ingress_url, &frame_prepared_request).await?;
    let frame_prepared_response =
        wait_for_terminal_result(storage.pool(), &frame_prepared_request.workflow_id).await?;
    assert_frame_switch_prepared_response(&frame_prepared_response)?;

    report_workflow_progress("e2e-frame-switch-crash", "starting");
    let frame_crash_response = drive_frame_switch_crash_process(storage).await?;
    report_workflow_progress("e2e-frame-switch-crash", "completed");
    assert_frame_switch_crash_response(&frame_crash_response)?;

    let frame_cancel_request = TurnRequest {
        workflow_id: "e2e-frame-switch-cancel".to_string(),
        fail_once: false,
        scenario: TurnScenario::FrameSwitchCancel,
        signal: None,
    };
    submit_workflow(ingress_url, &frame_cancel_request).await?;
    let frame_cancel_response =
        wait_for_terminal_result(storage.pool(), &frame_cancel_request.workflow_id).await?;
    assert_frame_switch_cancel_response(&frame_cancel_response)?;

    drive_suspended_sleep_cancel_scenario(storage, ingress_url, admin_url).await?;
    drive_engine_restart_scenario(storage, ingress_url, admin_url).await?;
    drive_turn_control_scenarios(storage, ingress_url).await?;
    drive_durable_wait_index_scenarios(storage, ingress_url, admin_url).await?;
    Ok(())
}
