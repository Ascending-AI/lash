use anyhow::{Context, Result};
use lash::triggers::{TriggerOccurrenceRequest, empty_trigger_source_key};
use lash_core::AwaitEventResolver as _;
use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, InlineRuntimeEffectController,
    Resolution, ScopedEffectController, SessionCommitStore, TurnAddress, TurnCancelOutcome,
    TurnCancelRequest, TurnOutcome, TurnStop, TurnTerminal, TurnWorkDriver,
};
use lash_postgres_store::PostgresStorage;
use lash_restate::{
    RestateAdminClient, RestateConnection, RestateEffectHost, RestateIngressClient,
    RestateInvocationId, RestateInvocationStatus, RestateProcessDeployment, RestateTurnDeployment,
};
use lash_restate_postgres_workers_e2e::{
    ATTACHMENT_MIME, BUTTON_SOURCE_TYPE, DEFAULT_SESSION_ID, DirectDurableWaitAwaitRequest,
    DirectDurableWaitAwaitResponse, DirectDurableWaitResolveRequest,
    DirectDurableWaitResolveResponse, EXPECTED_ASYNC_TEXT, EXPECTED_DURABLE_INPUT_TEXT,
    EXPECTED_FINAL_TEXT, EXPECTED_FRAME_SWITCH_CANCEL_TEXT, EXPECTED_FRAME_SWITCH_TEXT,
    EXPECTED_PARENT_DURABLE_INPUT_TEXT, EXPECTED_SEGMENT_LOOP_TEXT, EXPECTED_TOOL_BATCH_TEXT,
    ProcessSignalRequest, TURN_WORKFLOW_NAME, TurnRequest, TurnResponse, TurnScenario,
    build_e2e_core, e2e_tokio_thread_stack_bytes, ensure_e2e_schema, env,
    expected_attachment_bytes, process_registry_from_storage, record_terminal_result,
    reset_e2e_rows, s3_store_from_env, turn_session_id,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;

const DEFAULT_RUNNER_STALL_TIMEOUT: Duration = Duration::from_secs(240);

struct RunnerProgress {
    last_update: Instant,
    description: String,
}

fn runner_progress() -> &'static Mutex<RunnerProgress> {
    static PROGRESS: OnceLock<Mutex<RunnerProgress>> = OnceLock::new();
    PROGRESS.get_or_init(|| {
        Mutex::new(RunnerProgress {
            last_update: Instant::now(),
            description: "runner startup".to_string(),
        })
    })
}

fn report_workflow_progress(workflow_id: &str, phase: &str) {
    let description = format!("workflow={workflow_id} phase={phase}");
    {
        let mut progress = runner_progress().lock().expect("runner progress lock");
        progress.last_update = Instant::now();
        progress.description.clone_from(&description);
    }
    eprintln!(
        "[{}] workers-e2e progress: {description}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
}

fn runner_stall_timeout() -> Result<Duration> {
    let Some(raw) = std::env::var("LASH_E2E_STALL_TIMEOUT_SECS").ok() else {
        return Ok(DEFAULT_RUNNER_STALL_TIMEOUT);
    };
    let seconds = raw
        .parse::<u64>()
        .with_context(|| format!("LASH_E2E_STALL_TIMEOUT_SECS must be seconds, got `{raw}`"))?;
    anyhow::ensure!(seconds > 0, "LASH_E2E_STALL_TIMEOUT_SECS must be positive");
    Ok(Duration::from_secs(seconds))
}

fn main() -> Result<()> {
    let stack_bytes = e2e_tokio_thread_stack_bytes()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack_bytes)
        .build()
        .context("build e2e runner Tokio runtime")?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let database_url = lash_restate_postgres_workers_e2e::required_env("DATABASE_URL")?;
    let storage = wait_for_postgres(&database_url).await?;
    ensure_e2e_schema(storage.pool()).await?;
    reset_e2e_rows(storage.pool()).await?;

    let trace_dir = std::env::var("LASH_E2E_TRACE_DIR").ok().map(PathBuf::from);
    if let Some(dir) = &trace_dir {
        reset_trace_dir(dir)?;
    }

    let attachment_store = s3_store_from_env()?;
    wait_for_minio(&attachment_store).await?;
    let mock_provider_base_url = env("MOCK_PROVIDER_BASE_URL", "http://mock-provider:18001");
    wait_for_mock_provider(&mock_provider_base_url).await?;

    let admin_url = env("RESTATE_ADMIN_URL", "http://restate:9070");
    let deployment_url = env("WORKER_DEPLOYMENT_URL", "http://worker-proxy:18100");
    register_restate_deployment(&admin_url, &deployment_url).await?;
    let ingress_url = env("RESTATE_INGRESS_URL", "http://restate:8080");
    let watchdog = tokio::spawn(runner_stall_watchdog(
        storage.pool().clone(),
        admin_url.clone(),
        runner_stall_timeout()?,
    ));

    let conformance_ingress = ingress_url.clone();
    lash_core::testing::conformance::effect_host_await_events_cold_instance(|| {
        Arc::new(RestateEffectHost::new(conformance_ingress.clone()))
            as Arc<dyn lash_core::EffectHost>
    })
    .await;
    println!(
        "Restate cold-instance AwaitEvent conformance passed: layer_a_vectors={}",
        lash_core::testing::conformance::COLD_INSTANCE_AWAIT_EVENT_VECTOR_COUNT
    );
    run_cold_process_await_event_vectors(&admin_url, &ingress_url).await?;

    let main_request = TurnRequest {
        workflow_id: "e2e-main".to_string(),
        fail_once: false,
        scenario: TurnScenario::KitchenSink,
        signal: None,
    };
    submit_workflow(&ingress_url, &main_request).await?;
    let main_response = wait_for_terminal_result(storage.pool(), &main_request.workflow_id).await?;
    assert_kitchen_sink_response(&main_response, true)?;
    wait_for_queued_work(
        &storage,
        &mock_provider_base_url,
        trace_dir.clone(),
        &ingress_url,
    )
    .await?;
    let main_wake_request = TurnRequest {
        workflow_id: "e2e-main-wake".to_string(),
        fail_once: false,
        scenario: TurnScenario::DrainQueued,
        signal: None,
    };
    submit_workflow(&ingress_url, &main_wake_request).await?;
    let main_wake_response =
        wait_for_terminal_result(storage.pool(), &main_wake_request.workflow_id).await?;
    assert_queued_wake_response(&main_wake_response)?;

    let trigger_request = TurnRequest {
        workflow_id: "e2e-trigger-setup".to_string(),
        fail_once: false,
        scenario: TurnScenario::TriggerSetup,
        signal: None,
    };
    submit_workflow(&ingress_url, &trigger_request).await?;
    let trigger_setup_response =
        wait_for_terminal_result(storage.pool(), &trigger_request.workflow_id).await?;
    assert_trigger_setup_response(&trigger_setup_response)?;
    let trigger_process_id = emit_button_event(
        &storage,
        &mock_provider_base_url,
        trace_dir.clone(),
        &ingress_url,
    )
    .await?;
    wait_for_process_terminal(storage.pool(), &trigger_process_id).await?;

    let signal_setup_request = TurnRequest {
        workflow_id: "e2e-signal-suspend-setup".to_string(),
        fail_once: false,
        scenario: TurnScenario::SignalSuspend,
        signal: None,
    };
    submit_workflow(&ingress_url, &signal_setup_request).await?;
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
    submit_workflow(&ingress_url, &failover_request).await?;
    let failover_response =
        wait_for_terminal_result(storage.pool(), &failover_request.workflow_id).await?;
    assert_kitchen_sink_response(&failover_response, true)?;
    wait_for_process_signal_wait(storage.pool(), &signal_process_id, "first", 1).await?;
    wait_for_queued_work(
        &storage,
        &mock_provider_base_url,
        trace_dir.clone(),
        &ingress_url,
    )
    .await?;
    let failover_wake_request = TurnRequest {
        workflow_id: "e2e-failover-wake".to_string(),
        fail_once: false,
        scenario: TurnScenario::DrainQueued,
        signal: None,
    };
    submit_workflow(&ingress_url, &failover_wake_request).await?;
    let failover_wake_response =
        wait_for_terminal_result(storage.pool(), &failover_wake_request.workflow_id).await?;
    assert_queued_wake_response(&failover_wake_response)?;

    submit_signal_workflow(
        &ingress_url,
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
        &ingress_url,
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
    submit_workflow(&ingress_url, &async_request).await?;
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
        submit_workflow(&ingress_url, &request).await?;
        let response = wait_for_terminal_result(storage.pool(), workflow_id).await?;
        assert_process_llm_query_response(&response)?;
    }

    let durable_input_request = TurnRequest {
        workflow_id: "e2e-durable-input".to_string(),
        fail_once: false,
        scenario: TurnScenario::DurableInputRequest,
        signal: None,
    };
    submit_workflow(&ingress_url, &durable_input_request).await?;
    let (durable_key, durable_waiter_worker) =
        wait_for_durable_input_key(storage.pool(), &durable_input_request.workflow_id).await?;
    wait_for_durable_wait_attached(&admin_url, &durable_key).await?;
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
    submit_workflow(&ingress_url, &parent_durable_input_request).await?;
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
    run_engine_promise_conformance(&admin_url, &ingress_url).await?;
    println!(
        "durable-wait wake gates passed: waiter-before-resolution; resolution-before-waiter; peer-worker resolution across failover"
    );
    if std::env::var("LASH_E2E_WAKE_RCA_ONLY").as_deref() == Ok("1") {
        assert_durable_input_attempts(storage.pool()).await?;
        println!(
            "wake RCA soak passed: failover; queued-drain; waiter-before-resolution; resolution-before-waiter"
        );
        watchdog.abort();
        return Ok(());
    }
    assert_no_active_lash_restate_invocations(&admin_url).await?;
    assert_no_problem_lash_restate_invocations(&admin_url).await?;

    let tool_batch_request = TurnRequest {
        workflow_id: "e2e-tool-batch".to_string(),
        fail_once: false,
        scenario: TurnScenario::ToolBatch,
        signal: None,
    };
    submit_workflow(&ingress_url, &tool_batch_request).await?;
    let tool_batch_response =
        wait_for_terminal_result(storage.pool(), &tool_batch_request.workflow_id).await?;
    assert_tool_batch_response(&tool_batch_response)?;

    let tool_batch_failover_request = TurnRequest {
        workflow_id: "e2e-tool-batch-failover".to_string(),
        fail_once: true,
        scenario: TurnScenario::ToolBatch,
        signal: None,
    };
    submit_workflow(&ingress_url, &tool_batch_failover_request).await?;
    let tool_batch_failover_response =
        wait_for_terminal_result(storage.pool(), &tool_batch_failover_request.workflow_id).await?;
    assert_tool_batch_response(&tool_batch_failover_response)?;

    let segment_loop_request = TurnRequest {
        workflow_id: "e2e-segment-loop".to_string(),
        fail_once: false,
        scenario: TurnScenario::SegmentLoop,
        signal: None,
    };
    submit_workflow(&ingress_url, &segment_loop_request).await?;
    let segment_loop_response =
        wait_for_terminal_result(storage.pool(), &segment_loop_request.workflow_id).await?;
    assert_segment_loop_response(&segment_loop_response)?;

    let frame_queued_request = TurnRequest {
        workflow_id: "e2e-frame-switch-queued".to_string(),
        fail_once: false,
        scenario: TurnScenario::FrameSwitchQueued,
        signal: None,
    };
    submit_workflow(&ingress_url, &frame_queued_request).await?;
    let frame_queued_response =
        wait_for_terminal_result(storage.pool(), &frame_queued_request.workflow_id).await?;
    assert_frame_switch_queued_response(&frame_queued_response)?;

    let frame_prepared_request = TurnRequest {
        workflow_id: "e2e-frame-switch-prepared".to_string(),
        fail_once: false,
        scenario: TurnScenario::FrameSwitchPrepared,
        signal: None,
    };
    submit_workflow(&ingress_url, &frame_prepared_request).await?;
    let frame_prepared_response =
        wait_for_terminal_result(storage.pool(), &frame_prepared_request.workflow_id).await?;
    assert_frame_switch_prepared_response(&frame_prepared_response)?;

    report_workflow_progress("e2e-frame-switch-crash", "starting");
    let frame_crash_response = drive_frame_switch_crash_process(&storage).await?;
    report_workflow_progress("e2e-frame-switch-crash", "completed");
    assert_frame_switch_crash_response(&frame_crash_response)?;

    let frame_cancel_request = TurnRequest {
        workflow_id: "e2e-frame-switch-cancel".to_string(),
        fail_once: false,
        scenario: TurnScenario::FrameSwitchCancel,
        signal: None,
    };
    submit_workflow(&ingress_url, &frame_cancel_request).await?;
    let frame_cancel_response =
        wait_for_terminal_result(storage.pool(), &frame_cancel_request.workflow_id).await?;
    assert_frame_switch_cancel_response(&frame_cancel_response)?;

    drive_suspended_sleep_cancel_scenario(&storage, &ingress_url, &admin_url).await?;
    drive_engine_restart_scenario(&storage, &ingress_url, &admin_url).await?;
    drive_turn_control_scenarios(&storage, &ingress_url).await?;
    drive_durable_wait_index_scenarios(&ingress_url, &admin_url).await?;

    let responses = wait_for_terminal_results(storage.pool(), 26).await?;

    assert_processes_terminal(storage.pool()).await?;
    assert_no_duplicate_runtime_rows(storage.pool()).await?;
    assert_worker_distribution(storage.pool()).await?;
    assert_failover(storage.pool()).await?;
    assert_provider_calls(storage.pool()).await?;
    assert_frame_switch_provider_order(storage.pool()).await?;
    assert_tool_and_turn_telemetry(storage.pool()).await?;
    assert_tool_batch_side_effects(storage.pool()).await?;
    assert_durable_input_attempts(storage.pool()).await?;
    assert_trigger_delivery(storage.pool(), &trigger_process_id).await?;
    assert_attachments_round_trip(storage.pool(), &attachment_store, &responses).await?;
    assert_reopened_session_agrees(
        &storage,
        &mock_provider_base_url,
        trace_dir.clone(),
        &ingress_url,
        &responses,
    )
    .await?;
    if let Some(dir) = &trace_dir {
        assert_traces(dir).await?;
    }
    drive_break_glass_scenario(&storage, &ingress_url, &admin_url).await?;
    assert_no_active_lash_restate_invocations(&admin_url).await?;

    println!(
        "restate-postgres-workers e2e passed: {} workflows; suspended-sleep gates: post-suspension-cancel; engine-restart gates: journal-replay, suspended-sleep-cancel, post-restart-cancel-evidence, post-restart-completion; turn-control gates: cross-process, before-start, seal-race, crash-recovery, terminal-attach, break-glass-negative; trigger process {}; signal process {}; traces {}",
        responses.len(),
        trigger_process_id,
        signal_process_id,
        trace_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "disabled".to_string())
    );
    watchdog.abort();
    Ok(())
}

async fn run_cold_process_await_event_vectors(admin_url: &str, ingress_url: &str) -> Result<()> {
    for identity in ["tool_completion", "turn_cancel_gate"] {
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new("/usr/local/bin/lash-e2e-await-event-helper")
            .arg(ingress_url)
            .arg(identity)
            .arg(&nonce)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn cold-process helper for {identity}"))?;
        let stdout = child.stdout.take().context("helper stdout pipe")?;
        let mut lines = BufReader::new(stdout).lines();
        let encoded_key = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .with_context(|| format!("helper did not mint {identity} key"))??
            .with_context(|| format!("helper exited before printing {identity} key"))?;
        let key: AwaitEventKey = serde_json::from_str(&encoded_key)
            .with_context(|| format!("decode helper {identity} key"))?;
        wait_for_durable_wait_suspended(admin_url, &key).await?;
        child
            .kill()
            .await
            .with_context(|| format!("kill parked {identity} helper"))?;
        let status = child
            .wait()
            .await
            .with_context(|| format!("reap parked {identity} helper"))?;
        anyhow::ensure!(
            !status.success(),
            "killed {identity} helper exited successfully"
        );

        let terminal = Resolution::Ok(json!({
            "cold_process": true,
            "identity": identity,
            "nonce": nonce,
        }));
        let resolver = RestateEffectHost::new(ingress_url.to_string());
        anyhow::ensure!(
            matches!(
                resolver
                    .resolve_await_event(&key, terminal.clone())
                    .await
                    .with_context(|| format!("resolve killed-helper {identity} key"))?,
                lash_core::ResolveOutcome::Accepted
            ),
            "killed-helper {identity} resolution did not win"
        );
        let observer = RestateEffectHost::new(ingress_url.to_string());
        anyhow::ensure!(
            observer
                .peek_await_event(&key)
                .await
                .with_context(|| format!("peek killed-helper {identity} key"))?
                == Some(terminal.clone()),
            "cold observer did not see killed-helper {identity} terminal"
        );
        anyhow::ensure!(
            observer
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None,)
                .await
                .with_context(|| format!("observe killed-helper {identity} key"))?
                == terminal,
            "cold observer did not await killed-helper {identity} terminal"
        );
    }
    println!(
        "Restate cold-process AwaitEvent conformance passed: layer_b_vectors=2 identities=tool_completion,turn_cancel_gate"
    );
    Ok(())
}

async fn runner_stall_watchdog(pool: sqlx::PgPool, admin_url: String, timeout: Duration) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let stalled = {
            let progress = runner_progress().lock().expect("runner progress lock");
            (progress.last_update.elapsed() >= timeout)
                .then(|| (progress.last_update.elapsed(), progress.description.clone()))
        };
        let Some((elapsed, description)) = stalled else {
            continue;
        };

        eprintln!(
            "[{}] workers-e2e STALL: no workflow progress for {:.1}s; last progress: {}",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            elapsed.as_secs_f64(),
            description
        );
        dump_runner_stall_diagnostics(&pool, &admin_url).await;
        // This is an E2E-only binary. Exiting gives the compose wrapper a
        // nonzero status immediately; its EXIT trap then appends every service
        // log instead of waiting for the CI job timeout.
        std::process::exit(124);
    }
}

async fn dump_runner_stall_diagnostics(pool: &sqlx::PgPool, admin_url: &str) {
    let admin = RestateAdminClient::new(admin_url.to_string());
    match admin
        .unfinished_invocations_for_service_prefixes(&[
            TURN_WORKFLOW_NAME,
            "LashProcessWorkflow",
            "LashDurableWaitWorkflow",
        ])
        .await
    {
        Ok(invocations) => {
            eprintln!("workers-e2e STALL Restate unfinished invocations:\n{invocations:#?}")
        }
        Err(err) => eprintln!("workers-e2e STALL Restate query failed: {err:#}"),
    }

    let recent_events = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT workflow_id, worker_id, event_type, created_at_ms
         FROM lash_e2e_worker_events
         ORDER BY event_id DESC
         LIMIT 25",
    )
    .fetch_all(pool)
    .await;
    match recent_events {
        Ok(events) => eprintln!("workers-e2e STALL recent worker events:\n{events:#?}"),
        Err(err) => eprintln!("workers-e2e STALL worker-event query failed: {err:#}"),
    }
}

async fn dump_workflow_timeout_diagnostics(pool: &sqlx::PgPool, workflow_id: &str) {
    let session_id = turn_session_id(workflow_id);
    let admin_url = env("RESTATE_ADMIN_URL", "http://restate:9070");
    let ingress_url = env("RESTATE_INGRESS_URL", "http://restate:8080");
    eprintln!(
        "workers-e2e TIMEOUT SNAPSHOT workflow={workflow_id} session={session_id} (captured before 180s exit)"
    );

    let recorded_wait_rows = sqlx::query_as::<_, (String, String, String, String, String, i64)>(
        "SELECT workflow_id, worker_id, tool_name, args_json, result_json, created_at_ms
         FROM lash_e2e_tool_events
         WHERE workflow_id = $1 AND tool_name = 'durable_input_request.opened'
         ORDER BY event_id",
    )
    .bind(workflow_id)
    .fetch_all(pool)
    .await;
    let mut recorded_wait_keys = Vec::new();
    match &recorded_wait_rows {
        Ok(rows) => {
            eprintln!("workers-e2e TIMEOUT recorded await keys:\n{rows:#?}");
            for (_, _, _, _, result_json, _) in rows {
                let key = serde_json::from_str::<Value>(result_json)
                    .ok()
                    .and_then(|value| value.get("await_key").cloned())
                    .and_then(|value| serde_json::from_value::<AwaitEventKey>(value).ok());
                if let Some(key) = key {
                    recorded_wait_keys.push(key);
                }
            }
        }
        Err(err) => eprintln!("workers-e2e TIMEOUT await-key query failed: {err:#}"),
    }

    let process_ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT process_id
         FROM lash_process_events
         WHERE event_json LIKE $1
         ORDER BY process_id",
    )
    .bind(format!("%{workflow_id}%"))
    .fetch_all(pool)
    .await;
    let mut process_ids = match process_ids {
        Ok(process_ids) => process_ids,
        Err(err) => {
            eprintln!("workers-e2e TIMEOUT process-id query failed: {err:#}");
            Vec::new()
        }
    };
    for key in &recorded_wait_keys {
        if let ExecutionScope::Process { process_id } = &key.scope {
            process_ids.push(process_id.clone());
        }
    }
    process_ids.sort();
    process_ids.dedup();

    if !process_ids.is_empty() {
        match sqlx::query_as::<_, (String, String, i64, i64, String)>(
            "SELECT process_id, status, created_at_ms, updated_at_ms, record_json
             FROM lash_processes
             WHERE process_id = ANY($1)
             ORDER BY process_id",
        )
        .bind(&process_ids)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => eprintln!("workers-e2e TIMEOUT process state:\n{rows:#?}"),
            Err(err) => eprintln!("workers-e2e TIMEOUT process-state query failed: {err:#}"),
        }
        match sqlx::query_as::<_, (String, i64, String, i64, String)>(
            "SELECT process_id, sequence, event_type, occurred_at_ms, event_json
             FROM lash_process_events
             WHERE process_id = ANY($1)
             ORDER BY process_id, sequence",
        )
        .bind(&process_ids)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => eprintln!("workers-e2e TIMEOUT process events:\n{rows:#?}"),
            Err(err) => eprintln!("workers-e2e TIMEOUT process-event query failed: {err:#}"),
        }
        match sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                i64,
                i64,
                i64,
            ),
        >(
            "SELECT process_id, lease_owner_id, lease_owner_incarnation_id, lease_token,
                    lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms
             FROM lash_process_leases
             WHERE process_id = ANY($1)
             ORDER BY process_id",
        )
        .bind(&process_ids)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => eprintln!("workers-e2e TIMEOUT process lease rows:\n{rows:#?}"),
            Err(err) => eprintln!("workers-e2e TIMEOUT process-lease query failed: {err:#}"),
        }
    } else {
        eprintln!("workers-e2e TIMEOUT process state/events: no matching process ids");
    }

    match sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
            i64,
        ),
    >(
        "SELECT session_id, lease_owner_id, lease_owner_incarnation_id, lease_token,
                lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms
         FROM lash_session_execution_leases
         WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => eprintln!("workers-e2e TIMEOUT session lease row:\n{rows:#?}"),
        Err(err) => eprintln!("workers-e2e TIMEOUT session-lease query failed: {err:#}"),
    }

    let admin = RestateAdminClient::new(admin_url);
    let mut invocation_predicates = vec![
        format!(
            "(target_service_name = {} AND target_service_key = {})",
            sql_string_literal(TURN_WORKFLOW_NAME),
            sql_string_literal(workflow_id)
        ),
        format!(
            "(target_service_name = 'LashDurableWaitIndex' AND target_service_key = {})",
            sql_string_literal(session_id)
        ),
    ];
    invocation_predicates.extend(process_ids.iter().map(|process_id| {
        format!(
            "(target_service_name = 'LashProcessWorkflow' AND target_service_key = {})",
            sql_string_literal(process_id)
        )
    }));
    for key in &recorded_wait_keys {
        let address = lash_restate::RestateDurableWaitAddress::for_key(key);
        invocation_predicates.push(format!(
            "(target_service_name = 'LashDurableWaitWorkflow' AND target_service_key = {})",
            sql_string_literal(&address.workflow_key)
        ));
        invocation_predicates.push(format!(
            "(target_service_name = 'LashDurableWaitIndex' AND target_service_key = {})",
            sql_string_literal(&address.index_key())
        ));
    }
    let invocations = admin
        .query_json::<RestateInvocationStatus>(&format!(
            "SELECT id, target, target_service_name, target_service_key, target_handler_name, status, completion_result, completion_failure \
             FROM sys_invocation \
             WHERE {} \
             ORDER BY modified_at DESC",
            invocation_predicates.join(" OR ")
        ))
        .await;
    match &invocations {
        Ok(rows) => {
            eprintln!("workers-e2e TIMEOUT Restate relevant session invocations:\n{rows:#?}")
        }
        Err(err) => eprintln!("workers-e2e TIMEOUT Restate invocation query failed: {err:#}"),
    }

    let mut state_predicates = vec![format!(
        "(service_name = 'LashDurableWaitIndex' AND service_key = {})",
        sql_string_literal(session_id)
    )];
    for key in &recorded_wait_keys {
        let address = lash_restate::RestateDurableWaitAddress::for_key(key);
        state_predicates.push(format!(
            "(service_name = 'LashDurableWaitWorkflow' AND service_key = {})",
            sql_string_literal(&address.workflow_key)
        ));
        state_predicates.push(format!(
            "(service_name = 'LashDurableWaitIndex' AND service_key = {})",
            sql_string_literal(&address.index_key())
        ));
    }
    match admin
        .query_json::<Value>(&format!(
            "SELECT service_name, service_key, key, value_utf8 \
             FROM state \
             WHERE {} \
             ORDER BY service_name, service_key, key",
            state_predicates.join(" OR ")
        ))
        .await
    {
        Ok(rows) => eprintln!("workers-e2e TIMEOUT durable-wait workflow/index state:\n{rows:#?}"),
        Err(err) => eprintln!("workers-e2e TIMEOUT Restate state query failed: {err:#}"),
    }

    let wait_host = RestateEffectHost::new(ingress_url);
    let mut completed_promise_keys = BTreeSet::new();
    for key in &recorded_wait_keys {
        let workflow_key = lash_restate::RestateDurableWaitAddress::for_key(key).workflow_key;
        match wait_host.peek_await_event(key).await {
            Ok(Some(resolution)) => {
                completed_promise_keys.insert(workflow_key.clone());
                eprintln!(
                    "workers-e2e TIMEOUT durable promise key={workflow_key} completed={resolution:?}"
                );
            }
            Ok(None) => {
                eprintln!("workers-e2e TIMEOUT durable promise key={workflow_key} pending")
            }
            Err(err) => eprintln!(
                "workers-e2e TIMEOUT durable promise key={workflow_key} peek failed: {err:#}"
            ),
        }
    }

    let discriminator = invocations.as_ref().ok().map(|rows| {
        let wait_rows = rows
            .iter()
            .filter(|row| {
                row.target_service_name == "LashDurableWaitWorkflow"
                    && row.target_handler_name == "await_resolution"
                    && row
                        .target_service_key
                        .as_ref()
                        .is_some_and(|key| {
                            recorded_wait_keys.iter().any(|recorded| {
                                lash_restate::RestateDurableWaitAddress::for_key(recorded)
                                    .workflow_key
                                    == *key
                            })
                        })
            })
            .collect::<Vec<_>>();
        let parent_running_or_backing_off = rows.iter().any(|row| {
            (row.target_service_name == TURN_WORKFLOW_NAME
                && row.target_service_key.as_deref() == Some(workflow_id)
                || row.target_service_name == "LashProcessWorkflow"
                    && process_ids
                        .iter()
                        .any(|process_id| row.target_service_key.as_deref() == Some(process_id)))
                && matches!(row.status.as_str(), "ready" | "running" | "backing-off")
        });
        let callees_completed = !wait_rows.is_empty()
            && wait_rows
                .iter()
                .all(|row| row.status == "completed" && row.completion_result.as_deref() == Some("success"));
        let suspended_on_completed_promise = wait_rows.iter().any(|row| {
            row.status == "suspended"
                && row
                    .target_service_key
                    .as_ref()
                    .is_some_and(|key| completed_promise_keys.contains(key))
        });

        if callees_completed && parent_running_or_backing_off {
            "GUARD-HANG: await_resolution callees Completed while the parent is running/backing-off"
        } else if suspended_on_completed_promise {
            "PROMISE-STRANDING: await_resolution callee is suspended on a promise that peek reports completed"
        } else {
            "INCONCLUSIVE: invocation/promise state matches neither ratified RCA discriminator"
        }
    });
    eprintln!(
        "workers-e2e TIMEOUT DISCRIMINATOR: {}",
        discriminator.unwrap_or("INCONCLUSIVE: Restate invocation query unavailable")
    );
}

fn reset_trace_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create trace dir `{}`", dir.display()))?;
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("read trace dir `{}`", dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::remove_file(entry.path())
                .with_context(|| format!("remove stale trace `{}`", entry.path().display()))?;
        }
    }
    Ok(())
}

async fn wait_for_postgres(database_url: &str) -> Result<PostgresStorage> {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last_error = None;
    while Instant::now() < deadline {
        match PostgresStorage::connect(database_url).await {
            Ok(storage) => return Ok(storage),
            Err(err) => {
                last_error = Some(err.to_string());
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    anyhow::bail!(
        "Postgres did not become ready: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )
}

async fn drive_frame_switch_crash_process(storage: &PostgresStorage) -> Result<TurnResponse> {
    let binary = env(
        "LASH_E2E_FRAME_CRASH_BIN",
        "/usr/local/bin/lash-e2e-frame-crash",
    );
    for (mode, expected_code) in [("commit", 76), ("mid-follow", 77)] {
        let output = tokio::process::Command::new(&binary)
            .arg(mode)
            .output()
            .await
            .with_context(|| format!("run frame-crash subprocess mode `{mode}`"))?;
        anyhow::ensure!(
            output.status.code() == Some(expected_code),
            "frame-crash mode `{mode}` exited {:?}, expected {expected_code}; stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let output = tokio::process::Command::new(&binary)
        .arg("recover")
        .output()
        .await
        .context("run frame-crash recovery subprocess")?;
    anyhow::ensure!(
        output.status.success(),
        "frame-crash recovery failed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let final_value: Value =
        serde_json::from_slice(&output.stdout).context("decode frame-crash recovery result")?;
    let response = TurnResponse {
        workflow_id: "e2e-frame-switch-crash".to_string(),
        worker_id: "frame-crash-subprocess".to_string(),
        process_id: String::new(),
        process_ids: Vec::new(),
        attachment_id: String::new(),
        final_text: EXPECTED_FRAME_SWITCH_TEXT.to_string(),
        final_value,
        streamed_event_count: 0,
        replay_cursor: None,
        queued_turn_ran: true,
    };
    record_terminal_result(storage.pool(), &response).await?;
    Ok(response)
}

async fn wait_for_minio(store: &impl lash::persistence::AttachmentStore) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    let meta = lash_core::AttachmentCreateMeta::new(
        lash_core::MediaType::parse("image/png").unwrap(),
        Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
        Some("runner-health.png".to_string()),
    );
    let mut last_error = None;
    while Instant::now() < deadline {
        match store
            .put(b"runner-minio-health".to_vec(), meta.clone())
            .await
        {
            Ok(reference) => match store.get(&reference.id).await {
                Ok(stored) if stored.bytes == b"runner-minio-health" => return Ok(()),
                Ok(_) => last_error = Some("MinIO health attachment bytes changed".to_string()),
                Err(err) => last_error = Some(err.to_string()),
            },
            Err(err) => last_error = Some(err.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!(
        "MinIO did not become ready: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )
}

async fn register_restate_deployment(admin_url: &str, deployment_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .context("build Restate admin client")?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last_error = None;
    while Instant::now() < deadline {
        match client
            .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
            .json(&json!({
                "uri": deployment_url,
                "force": true,
                "breaking": true,
            }))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(format!("{status}: {body}"));
            }
            Err(err) => last_error = Some(err.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!(
        "Restate deployment registration failed: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )
}

async fn wait_for_mock_provider(base_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last_error = None;
    while Instant::now() < deadline {
        match client
            .get(format!("{}/health", base_url.trim_end_matches('/')))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(format!("{status}: {body}"));
            }
            Err(err) => last_error = Some(err.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!(
        "mock provider did not become ready: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )
}

async fn submit_workflow(ingress_url: &str, request: &TurnRequest) -> Result<RestateInvocationId> {
    report_workflow_progress(&request.workflow_id, "submitting");
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .context("build Restate ingress client")?;
    let ingress = RestateIngressClient::new(RestateConnection::with_client(ingress_url, client));
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_error = None;
    while Instant::now() < deadline {
        match ingress
            .send_workflow_json(TURN_WORKFLOW_NAME, &request.workflow_id, "run", request)
            .await
        {
            Ok(invocation_id) => {
                report_workflow_progress(&request.workflow_id, "submitted");
                return Ok(invocation_id);
            }
            Err(err) => last_error = Some(err.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!(
        "workflow `{}` submit failed: {}",
        request.workflow_id,
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )
}

async fn submit_signal_workflow(
    ingress_url: &str,
    pool: &sqlx::PgPool,
    workflow_id: &str,
    process_id: &str,
    signal_name: &str,
    signal_id: &str,
    payload: serde_json::Value,
) -> Result<TurnResponse> {
    let request = TurnRequest {
        workflow_id: workflow_id.to_string(),
        fail_once: false,
        scenario: TurnScenario::SignalProcess,
        signal: Some(ProcessSignalRequest {
            process_id: process_id.to_string(),
            signal_name: signal_name.to_string(),
            signal_id: signal_id.to_string(),
            payload,
        }),
    };
    submit_workflow(ingress_url, &request).await?;
    let response = wait_for_terminal_result(pool, workflow_id).await?;
    anyhow::ensure!(
        response
            .final_value
            .get("signalled")
            .and_then(Value::as_bool)
            == Some(true),
        "signal workflow `{workflow_id}` did not submit signalled=true: {}",
        response.final_value
    );
    Ok(response)
}

async fn assert_no_active_lash_restate_invocations(admin_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .context("build Restate admin client")?;
    let admin = RestateAdminClient::new(RestateConnection::with_client(admin_url, client));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let active = admin
            .unfinished_invocations_for_service_prefixes(&[
                TURN_WORKFLOW_NAME,
                "LashProcessWorkflow",
            ])
            .await
            .context("query Restate active Lash invocations")?;
        if active.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("Restate still has active Lash invocations: {active:#?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn assert_no_problem_lash_restate_invocations(admin_url: &str) -> Result<()> {
    // The break-glass negative gate deliberately cancels this exact Restate
    // invocation. Its dedicated assertion requires an engine failure and no
    // Lash `Cancelled` terminal, so exclude it from the general health sweep.
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .context("build Restate admin client")?;
    let admin = RestateAdminClient::new(RestateConnection::with_client(admin_url, client));
    let service_filter = [TURN_WORKFLOW_NAME, "LashProcessWorkflow"]
        .into_iter()
        .map(|prefix| {
            format!(
                "target_service_name LIKE {}",
                sql_string_literal(&format!("{prefix}%"))
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let problems = admin
        .query_json::<RestateInvocationStatus>(&format!(
            "SELECT id, target, target_service_name, target_service_key, target_handler_name, status, completion_result, completion_failure \
             FROM sys_invocation \
             WHERE ({service_filter}) \
               AND COALESCE(target_service_key, '') <> 'e2e-turn-break-glass' \
               AND (status IN ('backing-off', 'failed') OR completion_result = 'failure' OR completion_failure IS NOT NULL) \
             ORDER BY modified_at DESC"
        ))
        .await
        .context("query Restate problem Lash invocations")?;
    anyhow::ensure!(
        problems.is_empty(),
        "Restate has failed or backing-off Lash invocations: {problems:#?}"
    );
    Ok(())
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

async fn wait_for_terminal_result(pool: &sqlx::PgPool, workflow_id: &str) -> Result<TurnResponse> {
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        if let Some(response) = load_terminal_result(pool, workflow_id).await? {
            report_workflow_progress(workflow_id, "completed");
            return Ok(response);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    dump_workflow_timeout_diagnostics(pool, workflow_id).await;
    anyhow::bail!("timed out waiting for `{workflow_id}`")
}

async fn wait_for_terminal_results(
    pool: &sqlx::PgPool,
    expected: usize,
) -> Result<Vec<TurnResponse>> {
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        let rows = sqlx::query_as::<_, TerminalResultRow>(
            "SELECT workflow_id, process_id, worker_id, attachment_id, final_text, submitted_json,
                    queued_turn_ran, streamed_event_count, replay_cursor
             FROM lash_e2e_terminal_results
             ORDER BY workflow_id",
        )
        .fetch_all(pool)
        .await
        .context("load terminal results")?;
        if rows.len() >= expected {
            return rows
                .into_iter()
                .map(response_from_row)
                .collect::<Result<Vec<_>>>();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    dump_workflow_timeout_diagnostics(pool, "aggregate-terminal-results").await;
    anyhow::bail!("timed out waiting for {expected} completed workflows")
}

async fn load_terminal_result(
    pool: &sqlx::PgPool,
    workflow_id: &str,
) -> Result<Option<TurnResponse>> {
    sqlx::query_as::<_, TerminalResultRow>(
        "SELECT workflow_id, process_id, worker_id, attachment_id, final_text, submitted_json,
                queued_turn_ran, streamed_event_count, replay_cursor
         FROM lash_e2e_terminal_results
         WHERE workflow_id = $1",
    )
    .bind(workflow_id)
    .fetch_optional(pool)
    .await
    .context("load terminal result")?
    .map(response_from_row)
    .transpose()
}

type TerminalResultRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    bool,
    i64,
    Option<String>,
);

fn response_from_row(
    (
        workflow_id,
        process_id,
        worker_id,
        attachment_id,
        final_text,
        submitted_json,
        queued_turn_ran,
        streamed_event_count,
        replay_cursor,
    ): TerminalResultRow,
) -> Result<TurnResponse> {
    Ok(TurnResponse {
        workflow_id,
        worker_id,
        process_id,
        process_ids: Vec::new(),
        attachment_id,
        final_text,
        final_value: serde_json::from_str(&submitted_json)
            .with_context(|| format!("decode submitted JSON `{submitted_json}`"))?,
        streamed_event_count: streamed_event_count as usize,
        replay_cursor,
        queued_turn_ran,
    })
}

fn assert_kitchen_sink_response(response: &TurnResponse, expect_attachment: bool) -> Result<()> {
    anyhow::ensure!(
        response.final_text == EXPECTED_FINAL_TEXT,
        "workflow `{}` final text mismatch: {}",
        response.workflow_id,
        response.final_text
    );
    let submitted = &response.final_value;
    anyhow::ensure!(
        submitted.get("foreground").and_then(Value::as_str) == Some("lookup:foreground"),
        "foreground lookup missing from `{}`: {submitted}",
        response.workflow_id
    );
    anyhow::ensure!(
        submitted
            .pointer("/process/parent_lookup")
            .and_then(Value::as_str)
            == Some("lookup:parent"),
        "parent lookup missing from `{}`: {submitted}",
        response.workflow_id
    );
    anyhow::ensure!(
        submitted.pointer("/process/nested").and_then(Value::as_str) == Some("lookup:nested"),
        "nested process output missing from `{}`: {submitted}",
        response.workflow_id
    );
    anyhow::ensure!(
        submitted
            .pointer("/process/parallel/left")
            .and_then(Value::as_str)
            == Some("lookup:left")
            && submitted
                .pointer("/process/parallel/right")
                .and_then(Value::as_str)
                == Some("lookup:right"),
        "parallel process output missing from `{}`: {submitted}",
        response.workflow_id
    );
    anyhow::ensure!(
        submitted.pointer("/process/wake").and_then(Value::as_str) == Some("deferred"),
        "deferred wake marker missing from `{}`: {submitted}",
        response.workflow_id
    );
    if expect_attachment {
        anyhow::ensure!(
            !response.attachment_id.is_empty(),
            "workflow `{}` did not return an attachment id",
            response.workflow_id
        );
        anyhow::ensure!(
            submitted.get("attachment_mime").and_then(Value::as_str) == Some(ATTACHMENT_MIME),
            "workflow `{}` returned wrong attachment mime: {submitted}",
            response.workflow_id
        );
    }
    Ok(())
}

fn assert_queued_wake_response(response: &TurnResponse) -> Result<()> {
    anyhow::ensure!(
        response.final_text == lash_restate_postgres_workers_e2e::EXPECTED_WAKE_TEXT,
        "workflow `{}` wake final mismatch: {}",
        response.workflow_id,
        response.final_text
    );
    anyhow::ensure!(
        response
            .final_value
            .get("wake_consumed")
            .and_then(Value::as_bool)
            == Some(true),
        "queued wake workflow did not submit wake_consumed=true: {}",
        response.final_value
    );
    anyhow::ensure!(
        response.queued_turn_ran,
        "workflow `{}` did not run queued work",
        response.workflow_id
    );
    Ok(())
}

fn assert_trigger_setup_response(response: &TurnResponse) -> Result<()> {
    anyhow::ensure!(
        response
            .final_value
            .get("registered")
            .and_then(Value::as_bool)
            == Some(true),
        "trigger setup did not submit registered=true: {}",
        response.final_value
    );
    Ok(())
}

fn assert_signal_suspend_setup_response(response: &TurnResponse) -> Result<String> {
    anyhow::ensure!(
        response.final_value.get("final").and_then(Value::as_str) == Some("signal-suspend-started"),
        "signal setup did not submit signal-suspend-started: {}",
        response.final_value
    );
    let process_id = response
        .final_value
        .get("process_id")
        .and_then(Value::as_str)
        .context("signal setup submitted no process_id")?;
    Ok(process_id.to_string())
}

fn assert_async_completion_response(response: &TurnResponse) -> Result<()> {
    anyhow::ensure!(
        response.final_text == EXPECTED_ASYNC_TEXT,
        "workflow `{}` async final mismatch: {}",
        response.workflow_id,
        response.final_text
    );
    let async_value = response
        .final_value
        .get("async")
        .context("async completion response missing async result")?;
    anyhow::ensure!(
        async_value.get("async").and_then(Value::as_bool) == Some(true),
        "async completion result did not mark async=true: {}",
        response.final_value
    );
    anyhow::ensure!(
        async_value.get("value").and_then(Value::as_str) == Some("async:detached"),
        "async completion value mismatch: {}",
        response.final_value
    );
    Ok(())
}

fn assert_durable_input_response(response: &TurnResponse) -> Result<()> {
    anyhow::ensure!(
        response.final_text == EXPECTED_DURABLE_INPUT_TEXT,
        "workflow `{}` durable input final mismatch: {}",
        response.workflow_id,
        response.final_text
    );
    let durable = response
        .final_value
        .get("durable")
        .context("durable input response missing durable result")?;
    anyhow::ensure!(
        durable.get("answer").and_then(Value::as_str) == Some("durable-approved"),
        "durable input answer mismatch: {}",
        response.final_value
    );
    anyhow::ensure!(
        durable
            .get("request_id")
            .and_then(Value::as_str)
            .is_some_and(|request_id| request_id.ends_with(":request-1")),
        "durable input request id mismatch: {}",
        response.final_value
    );
    Ok(())
}

fn assert_process_llm_query_response(response: &TurnResponse) -> Result<()> {
    anyhow::ensure!(
        response.final_text == "process-llm-query-complete",
        "workflow `{}` process llm_query final mismatch: {}",
        response.workflow_id,
        response.final_text
    );
    anyhow::ensure!(
        response.final_value.get("category").and_then(Value::as_str) == Some("personal"),
        "workflow `{}` process llm_query category mismatch: {}",
        response.workflow_id,
        response.final_value
    );
    anyhow::ensure!(
        response
            .final_value
            .get("confidence")
            .and_then(Value::as_f64)
            == Some(0.98),
        "workflow `{}` process llm_query confidence mismatch: {}",
        response.workflow_id,
        response.final_value
    );
    Ok(())
}

fn assert_parent_durable_input_response(response: &TurnResponse) -> Result<()> {
    anyhow::ensure!(
        response.final_text == EXPECTED_PARENT_DURABLE_INPUT_TEXT,
        "workflow `{}` parent durable input final mismatch: {}",
        response.workflow_id,
        response.final_text
    );
    let parent = response
        .final_value
        .get("parent")
        .context("parent durable input response missing parent result")?;
    anyhow::ensure!(
        parent.get("child").and_then(Value::as_str) == Some("ready"),
        "parent child result mismatch: {}",
        response.final_value
    );
    let durable = parent
        .get("durable")
        .context("parent durable input response missing durable result")?;
    anyhow::ensure!(
        durable.get("answer").and_then(Value::as_str) == Some("parent-approved"),
        "parent durable input answer mismatch: {}",
        response.final_value
    );
    anyhow::ensure!(
        durable
            .get("request_id")
            .and_then(Value::as_str)
            .is_some_and(|request_id| request_id.ends_with(":request-1")),
        "parent durable input request id mismatch: {}",
        response.final_value
    );
    Ok(())
}

fn assert_tool_batch_response(response: &TurnResponse) -> Result<()> {
    anyhow::ensure!(
        response.final_text == EXPECTED_TOOL_BATCH_TEXT,
        "workflow `{}` tool-batch final mismatch: {}",
        response.workflow_id,
        response.final_text
    );
    let batch = response
        .final_value
        .get("batch")
        .context("tool-batch response missing batch result")?;
    anyhow::ensure!(
        batch.pointer("/slow/value").and_then(Value::as_str) == Some("batch:slow"),
        "tool-batch slow result mismatch: {}",
        response.final_value
    );
    anyhow::ensure!(
        batch.pointer("/fast/value").and_then(Value::as_str) == Some("batch:fast"),
        "tool-batch fast result mismatch: {}",
        response.final_value
    );
    anyhow::ensure!(
        batch.pointer("/literal").and_then(Value::as_str) == Some("kept"),
        "tool-batch literal result missing: {}",
        response.final_value
    );
    let keys = [
        batch.pointer("/slow/key").and_then(Value::as_str),
        batch.pointer("/fast/key").and_then(Value::as_str),
    ];
    anyhow::ensure!(
        keys == [Some("slow"), Some("fast")],
        "tool-batch result order was not source order: {}",
        response.final_value
    );
    Ok(())
}

fn assert_segment_loop_response(response: &TurnResponse) -> Result<()> {
    anyhow::ensure!(
        response.final_text == EXPECTED_SEGMENT_LOOP_TEXT,
        "workflow `{}` segmented-loop final mismatch: {}",
        response.workflow_id,
        response.final_text
    );
    let control = response
        .final_value
        .get("control")
        .context("segmented-loop response missing non-segmenting control")?;
    let segmented = response
        .final_value
        .get("segmented")
        .context("segmented-loop response missing segmented result")?;
    anyhow::ensure!(
        segmented == control,
        "segmentation changed the authored loop result: {}",
        response.final_value
    );
    anyhow::ensure!(
        segmented.get("total").and_then(Value::as_i64) == Some(28),
        "segmented loop did not execute all iterations: {}",
        response.final_value
    );
    anyhow::ensure!(
        segmented
            .get("values")
            .and_then(Value::as_array)
            .is_some_and(|values| values.len() == 8),
        "segmented loop observable effect sequence has the wrong length: {}",
        response.final_value
    );
    Ok(())
}

fn assert_frame_switch_queued_response(response: &TurnResponse) -> Result<()> {
    let value = &response.final_value;
    anyhow::ensure!(
        response.final_text == EXPECTED_FRAME_SWITCH_TEXT
            && value.get("seed_visible").and_then(Value::as_str)
                == Some("seed:e2e-frame-switch-queued")
            && value.get("follow_on").and_then(Value::as_bool) == Some(true),
        "queued frame-switch seed/follow-on mismatch: {value}"
    );
    for field in [
        "first_completed",
        "second_pending_before_drain",
        "second_completed",
        "queue_empty",
        "inputs_empty",
    ] {
        anyhow::ensure!(
            value.get(field).and_then(Value::as_bool) == Some(true),
            "queued frame-switch invariant `{field}` failed: {value}"
        );
    }
    Ok(())
}

fn assert_frame_switch_crash_response(response: &TurnResponse) -> Result<()> {
    let value = &response.final_value;
    anyhow::ensure!(
        response.final_text == EXPECTED_FRAME_SWITCH_TEXT
            && value.get("seed_visible").and_then(Value::as_str)
                == Some("seed:e2e-frame-switch-crash")
            && value.get("follow_on").and_then(Value::as_bool) == Some(true)
            && value
                .get("recovered_after_commit_exit")
                .and_then(Value::as_bool)
                == Some(true)
            && value
                .get("mid_follow_on_recovered")
                .and_then(Value::as_bool)
                == Some(true)
            && value.get("queue_empty").and_then(Value::as_bool) == Some(true)
            && value.get("inputs_empty").and_then(Value::as_bool) == Some(true),
        "crash-recovered frame-switch invariant mismatch: {value}"
    );
    Ok(())
}

fn assert_frame_switch_prepared_response(response: &TurnResponse) -> Result<()> {
    let value = &response.final_value;
    anyhow::ensure!(
        response.final_text == EXPECTED_FRAME_SWITCH_TEXT
            && value.get("seed_visible").and_then(Value::as_str)
                == Some("seed:e2e-frame-switch-prepared")
            && value.get("follow_on").and_then(Value::as_bool) == Some(true),
        "prepared frame-switch did not follow the task with its seed: {value}"
    );
    Ok(())
}

fn assert_frame_switch_cancel_response(response: &TurnResponse) -> Result<()> {
    let value = &response.final_value;
    anyhow::ensure!(
        response.final_text == EXPECTED_FRAME_SWITCH_CANCEL_TEXT
            && value.get("terminal_cancelled").and_then(Value::as_bool) == Some(true)
            && value.get("cancel_count").and_then(Value::as_u64) == Some(1)
            && value.get("claims_settled").and_then(Value::as_bool) == Some(true)
            && value.get("session_usable").and_then(Value::as_bool) == Some(true),
        "mid-chain cancellation invariant mismatch: {value}"
    );
    Ok(())
}

async fn assert_frame_switch_provider_order(pool: &sqlx::PgPool) -> Result<()> {
    let queued: Vec<String> = sqlx::query_scalar(
        "SELECT scenario FROM lash_e2e_provider_calls
         WHERE workflow_id = 'e2e-frame-switch-queued'
         ORDER BY call_id",
    )
    .fetch_all(pool)
    .await
    .context("load queued frame-switch provider order")?;
    anyhow::ensure!(
        queued
            == [
                "frame_switch_queued_start",
                "frame_switch_queued_follow",
                "frame_switch_pending",
            ],
        "queued frame-switch provider order changed: {queued:?}"
    );
    let crash: Vec<String> = sqlx::query_scalar(
        "SELECT scenario FROM lash_e2e_provider_calls
         WHERE workflow_id = 'e2e-frame-switch-crash'
         ORDER BY call_id",
    )
    .fetch_all(pool)
    .await
    .context("load crash frame-switch provider calls")?;
    anyhow::ensure!(
        crash == ["frame_switch_crash_start", "frame_switch_crash_follow"],
        "crash recovery duplicated or lost a physical turn: {crash:?}"
    );
    let prepared: Vec<String> = sqlx::query_scalar(
        "SELECT scenario FROM lash_e2e_provider_calls
         WHERE workflow_id = 'e2e-frame-switch-prepared'
         ORDER BY call_id",
    )
    .fetch_all(pool)
    .await
    .context("load prepared frame-switch provider calls")?;
    anyhow::ensure!(
        prepared
            == [
                "frame_switch_prepared_start",
                "frame_switch_prepared_follow"
            ],
        "prepared frame-switch provider order changed: {prepared:?}"
    );
    let cancel: Vec<String> = sqlx::query_scalar(
        "SELECT scenario FROM lash_e2e_provider_calls
         WHERE workflow_id = 'e2e-frame-switch-cancel'
         ORDER BY call_id",
    )
    .fetch_all(pool)
    .await
    .context("load cancelled frame-switch provider calls")?;
    anyhow::ensure!(
        cancel.first().map(String::as_str) == Some("frame_switch_cancel_start")
            && cancel.last().map(String::as_str) == Some("frame_switch_post_cancel")
            && cancel.len() >= 3
            && cancel[1..cancel.len() - 1]
                .iter()
                .all(|scenario| scenario == "frame_switch_cancel_follow"),
        "cancelled frame-switch provider order changed: {cancel:?}"
    );
    Ok(())
}

async fn wait_for_durable_input_key(
    pool: &sqlx::PgPool,
    workflow_id: &str,
) -> Result<(AwaitEventKey, String)> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT worker_id, result_json
             FROM lash_e2e_tool_events
             WHERE workflow_id = $1 AND tool_name = 'durable_input_request.opened'
             ORDER BY event_id DESC
             LIMIT 1",
        )
        .bind(workflow_id)
        .fetch_optional(pool)
        .await
        .with_context(|| format!("load durable input key for `{workflow_id}`"))?;
        if let Some((worker_id, result_json)) = row {
            let value: Value = serde_json::from_str(&result_json)
                .with_context(|| format!("decode durable input key row for `{workflow_id}`"))?;
            let key_value = value
                .get("await_key")
                .cloned()
                .context("durable input key row missing await_key")?;
            let key: AwaitEventKey =
                serde_json::from_value(key_value).context("decode durable input AwaitEventKey")?;
            return Ok((key, worker_id));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("timed out waiting for durable input key for `{workflow_id}`")
}

async fn wait_for_durable_wait_suspended(admin_url: &str, key: &AwaitEventKey) -> Result<()> {
    let workflow_key = lash_restate::RestateDurableWaitAddress::for_key(key).workflow_key;
    let admin = RestateAdminClient::new(admin_url.to_string());
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last_status = None;
    while Instant::now() < deadline {
        last_status = admin
            .workflow_invocation_status(
                "LashDurableWaitWorkflow",
                &workflow_key,
                "await_resolution",
            )
            .await
            .context("query durable wait invocation before peer resolution")?;
        if last_status
            .as_ref()
            .is_some_and(|status| status.status == "suspended")
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "durable wait `{workflow_key}` did not suspend before peer resolution; last status={last_status:?}"
    )
}

async fn wait_for_durable_wait_attached(admin_url: &str, key: &AwaitEventKey) -> Result<()> {
    let workflow_key = lash_restate::RestateDurableWaitAddress::for_key(key).workflow_key;
    let admin = RestateAdminClient::new(admin_url.to_string());
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_status = None;
    while Instant::now() < deadline {
        last_status = admin
            .workflow_invocation_status(
                "LashDurableWaitWorkflow",
                &workflow_key,
                "await_resolution",
            )
            .await
            .context("query durable wait invocation before peer resolution")?;
        if last_status.is_some() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "durable wait `{workflow_key}` did not attach before peer resolution; last status={last_status:?}"
    )
}

async fn engine_conformance_key(host: &RestateEffectHost, ordering: &str) -> Result<AwaitEventKey> {
    host.await_event_key(
        &ExecutionScope::runtime_operation(format!("workers-e2e-promise-conformance-{ordering}")),
        AwaitEventWaitIdentity::Custom {
            key: ordering.to_string(),
        },
    )
    .await
    .with_context(|| format!("build `{ordering}` engine conformance key"))
}

async fn await_durable_wait_on_worker(
    worker_id: &str,
    key: AwaitEventKey,
) -> Result<DirectDurableWaitAwaitResponse> {
    reqwest::Client::new()
        .post(format!("http://{worker_id}:18101/await-durable-wait"))
        .json(&DirectDurableWaitAwaitRequest { key })
        .send()
        .await
        .with_context(|| format!("call `{worker_id}` durable-wait await control endpoint"))?
        .error_for_status()
        .with_context(|| format!("`{worker_id}` durable-wait await status"))?
        .json::<DirectDurableWaitAwaitResponse>()
        .await
        .with_context(|| format!("decode `{worker_id}` durable-wait await response"))
}

async fn run_engine_promise_conformance(admin_url: &str, ingress_url: &str) -> Result<()> {
    let key_host = RestateEffectHost::new(ingress_url.to_string());
    let attached_key = engine_conformance_key(&key_host, "waiter-before-resolution").await?;
    let attached_expected = Resolution::Ok(json!({ "ordering": "waiter-before-resolution" }));
    let attached_wait_key = attached_key.clone();
    let attached_waiter =
        tokio::spawn(
            async move { await_durable_wait_on_worker("worker-a", attached_wait_key).await },
        );
    wait_for_durable_wait_suspended(admin_url, &attached_key).await?;
    let attached_outcome =
        resolve_durable_wait_from_peer_worker("worker-a", &attached_key, attached_expected.clone())
            .await?;
    anyhow::ensure!(
        matches!(attached_outcome, lash_core::ResolveOutcome::Accepted),
        "engine conformance attached resolution was not accepted: {attached_outcome:?}"
    );
    let attached_response = tokio::time::timeout(Duration::from_secs(30), attached_waiter)
        .await
        .context("attached engine conformance waiter did not wake")?
        .context("join attached engine conformance waiter")??;
    anyhow::ensure!(
        attached_response.worker_id == "worker-a"
            && attached_response.resolution == attached_expected,
        "attached engine conformance response mismatch: {attached_response:?}"
    );

    let pre_resolved_key = engine_conformance_key(&key_host, "resolution-before-waiter").await?;
    let pre_resolved_expected = Resolution::Ok(json!({ "ordering": "resolution-before-waiter" }));
    let pre_resolved_outcome = resolve_durable_wait_from_peer_worker(
        "worker-a",
        &pre_resolved_key,
        pre_resolved_expected.clone(),
    )
    .await?;
    anyhow::ensure!(
        matches!(pre_resolved_outcome, lash_core::ResolveOutcome::Accepted),
        "engine conformance pre-resolution was not accepted: {pre_resolved_outcome:?}"
    );
    let pre_resolved_response = tokio::time::timeout(
        Duration::from_secs(30),
        await_durable_wait_on_worker("worker-a", pre_resolved_key),
    )
    .await
    .context("pre-resolved engine conformance waiter did not return")??;
    anyhow::ensure!(
        pre_resolved_response.worker_id == "worker-a"
            && pre_resolved_response.resolution == pre_resolved_expected,
        "pre-resolved engine conformance response mismatch: {pre_resolved_response:?}"
    );
    println!(
        "promise engine conformance passed: suspended waiter=worker-a; resolver=worker-b; waiter-before-resolution; resolution-before-waiter"
    );
    Ok(())
}

async fn resolve_durable_wait_from_peer_worker(
    waiter_worker_id: &str,
    key: &AwaitEventKey,
    resolution: Resolution,
) -> Result<lash_core::ResolveOutcome> {
    let resolver_worker_id = match waiter_worker_id {
        "worker-a" => "worker-b",
        "worker-b" => "worker-a",
        other => anyhow::bail!("durable waiter ran on unexpected worker `{other}`"),
    };
    wait_for_worker_control_healthy(resolver_worker_id).await?;
    let response = reqwest::Client::new()
        .post(format!(
            "http://{resolver_worker_id}:18101/resolve-durable-wait"
        ))
        .json(&DirectDurableWaitResolveRequest {
            key: key.clone(),
            resolution,
        })
        .send()
        .await
        .with_context(|| format!("ask `{resolver_worker_id}` to resolve durable wait"))?
        .error_for_status()
        .with_context(|| format!("`{resolver_worker_id}` durable-wait resolve status"))?
        .json::<DirectDurableWaitResolveResponse>()
        .await
        .with_context(|| format!("decode `{resolver_worker_id}` durable-wait resolve response"))?;
    anyhow::ensure!(
        response.worker_id == resolver_worker_id,
        "durable wait resolved on `{}` instead of peer `{resolver_worker_id}`",
        response.worker_id
    );
    eprintln!(
        "promise conformance: waiter={waiter_worker_id} resolver={} outcome={:?}",
        response.worker_id, response.outcome
    );
    Ok(response.outcome)
}

async fn wait_for_worker_control_healthy(worker_id: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://{worker_id}:18101/health");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_error = None;
    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => last_error = Some(format!("HTTP {}", response.status())),
            Err(err) => last_error = Some(err.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "worker control endpoint `{worker_id}` did not recover before peer resolution; last error={last_error:?}"
    )
}

async fn drive_turn_control_scenarios(storage: &PostgresStorage, ingress_url: &str) -> Result<()> {
    let deployment = RestateTurnDeployment::new(ingress_url.to_string());
    let driver = deployment.turn_work_driver();

    // A remote host can durably win the gate before any worker owns the turn.
    let before = turn_control_request("e2e-turn-cancel-before-start", false);
    let before_evidence_id = "e2e-cancel-before-start";
    let before_outcome = driver
        .request_cancel(cancel_request(
            turn_address(storage, &before).await?,
            before_evidence_id,
        ))
        .await
        .context("request cancellation before turn start")?;
    assert_requested(&before_outcome.outcome, before_evidence_id)?;
    submit_workflow(ingress_url, &before).await?;
    let before_terminal = driver
        .await_terminal(&turn_address(storage, &before).await?)
        .await
        .context("attach to cancel-before-start terminal")?;
    report_workflow_progress(&before.workflow_id, "terminal-attached");
    assert_cancelled_terminal(&before_terminal, before_evidence_id)?;
    assert_cancelled_response(
        &wait_for_terminal_result(storage.pool(), &before.workflow_id).await?,
        before_evidence_id,
    )?;

    // The runner owns no Lash session handle. It waits for the peer worker's
    // tool boundary, then addresses the durable gate directly.
    let cross = turn_control_request("e2e-turn-cancel-cross-process", false);
    submit_workflow(ingress_url, &cross).await?;
    wait_for_cancel_gate(storage.pool(), &cross.workflow_id).await?;
    let cross_evidence_id = "e2e-cancel-cross-process";
    let cross_outcome = driver
        .request_cancel(cancel_request(
            turn_address(storage, &cross).await?,
            cross_evidence_id,
        ))
        .await
        .context("request cross-process cancellation")?;
    assert_requested(&cross_outcome.outcome, cross_evidence_id)?;
    let cross_terminal = driver
        .await_terminal(&turn_address(storage, &cross).await?)
        .await
        .context("attach to cross-process terminal")?;
    report_workflow_progress(&cross.workflow_id, "terminal-attached");
    assert_cancelled_terminal(&cross_terminal, cross_evidence_id)?;
    assert_cancelled_response(
        &wait_for_terminal_result(storage.pool(), &cross.workflow_id).await?,
        cross_evidence_id,
    )?;

    // This live gate races workflow submission against cancellation. The
    // tighter already-running commit-time seal window is covered by the inline
    // unit race; either way, a requested cancel must commit Cancelled with the
    // same evidence, while a completion seal must commit a non-cancel terminal
    // without evidence.
    let race = TurnRequest {
        workflow_id: "e2e-turn-cancel-seal-race".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlComplete,
        signal: None,
    };
    let race_evidence_id = "e2e-cancel-seal-race";
    let race_cancel = cancel_request(turn_address(storage, &race).await?, race_evidence_id);
    let (submitted, race_outcome) = tokio::join!(
        submit_workflow(ingress_url, &race),
        driver.request_cancel(race_cancel),
    );
    submitted.context("submit completion/cancel race")?;
    let race_outcome = race_outcome.context("request completion/cancel race")?;
    let race_terminal = driver
        .await_terminal(&turn_address(storage, &race).await?)
        .await
        .context("attach to completion/cancel race terminal")?;
    report_workflow_progress(&race.workflow_id, "terminal-attached");
    match race_outcome.outcome {
        TurnCancelOutcome::Requested(_) | TurnCancelOutcome::AlreadyRequested(_) => {
            assert_cancelled_terminal(&race_terminal, race_evidence_id)?;
        }
        TurnCancelOutcome::CompletionWonRace => assert_non_cancel_terminal(&race_terminal)?,
        TurnCancelOutcome::UnknownOrRevoked => {
            anyhow::bail!("completion/cancel race unexpectedly targeted a revoked gate")
        }
    }
    let _ = wait_for_terminal_result(storage.pool(), &race.workflow_id).await?;

    // The first owner exits from crash_once. Cancellation lands while Restate
    // is recovering the invocation; the peer owner must replay the keyed gate.
    let recovery = turn_control_request("e2e-turn-cancel-crash-recovery", true);
    submit_workflow(ingress_url, &recovery).await?;
    let crashed_worker = wait_for_failover_marker(storage.pool(), &recovery.workflow_id).await?;
    let recovery_evidence_id = "e2e-cancel-crash-recovery";
    let recovery_outcome = driver
        .request_cancel(cancel_request(
            turn_address(storage, &recovery).await?,
            recovery_evidence_id,
        ))
        .await
        .context("request cancellation during owner recovery")?;
    assert_requested(&recovery_outcome.outcome, recovery_evidence_id)?;
    report_workflow_progress(&recovery.workflow_id, "cancel-requested-after-crash");
    let recovery_terminal = driver
        .await_terminal(&turn_address(storage, &recovery).await?)
        .await
        .context("attach to recovered cancellation terminal")?;
    report_workflow_progress(&recovery.workflow_id, "terminal-attached");
    assert_cancelled_terminal(&recovery_terminal, recovery_evidence_id)?;
    let recovery_response = wait_for_terminal_result(storage.pool(), &recovery.workflow_id).await?;
    assert_cancelled_response(&recovery_response, recovery_evidence_id)?;
    anyhow::ensure!(
        recovery_response.worker_id != crashed_worker,
        "stale owner `{crashed_worker}` completed the recovered turn"
    );

    println!(
        "turn-control gates passed: cross-process; cancel-before-start; seal-vs-cancel; owner-crash-recovery; terminal-attach-evidence"
    );
    Ok(())
}

/// Prove cancellation wakes a timer only after Restate reports the parent turn
/// suspended; the 300-second deadline is intentionally far outside the gate.
async fn drive_suspended_sleep_cancel_scenario(
    storage: &PostgresStorage,
    ingress_url: &str,
    admin_url: &str,
) -> Result<()> {
    let request = TurnRequest {
        workflow_id: "e2e-suspended-sleep-cancel".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlSleep,
        signal: None,
    };
    let invocation_id = submit_workflow(ingress_url, &request).await?;
    let admin = RestateAdminClient::new(admin_url.to_string());
    wait_for_invocation_suspended(&admin, &invocation_id, Duration::from_secs(90)).await?;
    report_workflow_progress(&request.workflow_id, "durable-sleep-suspended");

    let evidence_id = "e2e-cancel-suspended-sleep";
    let driver = RestateTurnDeployment::new(ingress_url.to_string()).turn_work_driver();
    let started = Instant::now();
    let receipt = driver
        .request_cancel(cancel_request(
            turn_address(storage, &request).await?,
            evidence_id,
        ))
        .await
        .context("request cancellation after durable sleep suspended")?;
    assert_requested(&receipt.outcome, evidence_id)?;
    let terminal = tokio::time::timeout(
        Duration::from_secs(10),
        driver.await_terminal(&turn_address(storage, &request).await?),
    )
    .await
    .context("suspended durable sleep did not wake within 10 seconds")??;
    assert_cancelled_terminal(&terminal, evidence_id)?;
    anyhow::ensure!(
        started.elapsed() < Duration::from_secs(10),
        "suspended sleep cancellation waited for the 300-second timer"
    );
    let response = wait_for_terminal_result(storage.pool(), &request.workflow_id).await?;
    assert_cancelled_response(&response, evidence_id)?;
    println!("suspended-sleep gates passed: post-suspension-cancel");
    Ok(())
}

/// Coordinate a real Restate service bounce with the shell harness while both
/// workers and this runner stay alive. The tool wait preserves the existing
/// start-gate replay proof, while the timer proves a resolved cancel promise
/// wakes a suspended parent after the engine returns.
async fn drive_engine_restart_scenario(
    storage: &PostgresStorage,
    ingress_url: &str,
    admin_url: &str,
) -> Result<()> {
    let driver = RestateTurnDeployment::new(ingress_url.to_string()).turn_work_driver();
    let parked = turn_control_request("e2e-engine-restart-cancel", false);
    submit_workflow(ingress_url, &parked).await?;
    let sleeping = TurnRequest {
        workflow_id: "e2e-engine-restart-suspended-sleep".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlSleep,
        signal: None,
    };
    let sleeping_invocation_id = submit_workflow(ingress_url, &sleeping).await?;
    let admin = RestateAdminClient::new(admin_url.to_string());
    wait_for_cancel_gate_attempts(storage.pool(), &parked.workflow_id, 1).await?;
    wait_for_invocation_suspended(&admin, &sleeping_invocation_id, Duration::from_secs(90)).await?;
    record_harness_signal(storage.pool(), "engine-restart-ready").await?;
    report_workflow_progress(&parked.workflow_id, "parked-before-engine-restart");

    wait_for_harness_signal(storage.pool(), "engine-restart-complete").await?;
    wait_for_restate_recovery(admin_url).await?;
    wait_for_cancel_gate_attempts(storage.pool(), &parked.workflow_id, 2).await?;
    wait_for_invocation_suspended(&admin, &sleeping_invocation_id, Duration::from_secs(30)).await?;
    report_workflow_progress(&parked.workflow_id, "journal-replayed-after-engine-restart");

    let sleep_evidence_id = "e2e-cancel-suspended-sleep-after-engine-restart";
    let sleep_cancel_started = Instant::now();
    let sleep_receipt = driver
        .request_cancel(cancel_request(
            turn_address(storage, &sleeping).await?,
            sleep_evidence_id,
        ))
        .await
        .context("request suspended sleep cancellation after Restate engine restart")?;
    assert_requested(&sleep_receipt.outcome, sleep_evidence_id)?;
    let sleep_terminal = tokio::time::timeout(
        Duration::from_secs(10),
        driver.await_terminal(&turn_address(storage, &sleeping).await?),
    )
    .await
    .context("post-restart suspended sleep did not wake within 10 seconds")??;
    assert_cancelled_terminal(&sleep_terminal, sleep_evidence_id)?;
    anyhow::ensure!(
        sleep_cancel_started.elapsed() < Duration::from_secs(10),
        "post-restart suspended sleep cancellation waited for the 300-second timer"
    );
    let sleep_response = wait_for_terminal_result(storage.pool(), &sleeping.workflow_id).await?;
    assert_cancelled_response(&sleep_response, sleep_evidence_id)?;

    let evidence_id = "e2e-cancel-after-engine-restart";
    let parked_address = turn_address(storage, &parked).await?;
    let receipt = driver
        .request_cancel(
            TurnCancelRequest::new(
                parked_address.clone(),
                evidence_id,
                Some("scripted-engine-restart-runner".to_string()),
            )
            .with_reason("cancel a replayed turn after the Restate engine restarted"),
        )
        .await
        .context("request cancellation after Restate engine restart")?;
    let evidence = match &receipt.outcome {
        TurnCancelOutcome::Requested(evidence) | TurnCancelOutcome::AlreadyRequested(evidence) => {
            evidence
        }
        other => anyhow::bail!("post-restart cancellation did not win: {other:?}"),
    };
    anyhow::ensure!(
        evidence.request_id == evidence_id
            && evidence.origin.as_deref() == Some("scripted-engine-restart-runner")
            && evidence.reason.as_deref()
                == Some("cancel a replayed turn after the Restate engine restarted"),
        "post-restart cancellation receipt lost evidence: {evidence:?}"
    );
    let terminal = driver
        .await_terminal(&parked_address)
        .await
        .context("attach to post-restart cancellation terminal")?;
    assert_engine_restart_cancelled_terminal(&terminal, evidence_id)?;
    let cancelled = wait_for_terminal_result(storage.pool(), &parked.workflow_id).await?;
    anyhow::ensure!(
        cancelled.final_text == "turn-control-cancelled"
            && cancelled.final_value["cancellation"]["request_id"] == evidence_id
            && cancelled.final_value["cancellation"]["origin"] == "scripted-engine-restart-runner",
        "post-restart worker result lost cancellation evidence: {cancelled:#?}"
    );

    let complete = TurnRequest {
        workflow_id: "e2e-engine-restart-complete".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlComplete,
        signal: None,
    };
    submit_workflow(ingress_url, &complete).await?;
    let complete_terminal = driver
        .await_terminal(&turn_address(storage, &complete).await?)
        .await
        .context("attach to post-restart completion terminal")?;
    assert_non_cancel_terminal(&complete_terminal)?;
    let completed = wait_for_terminal_result(storage.pool(), &complete.workflow_id).await?;
    anyhow::ensure!(
        completed.final_text == "turn-control-completed",
        "post-restart turn did not commit normally: {completed:#?}"
    );
    println!(
        "engine-restart gates passed: journal-replay; suspended-sleep-cancel; post-restart-cancel-evidence; post-restart-completion"
    );
    Ok(())
}

async fn record_harness_signal(pool: &sqlx::PgPool, signal_name: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO lash_e2e_harness_signals (signal_name, created_at_ms)
         VALUES ($1, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT)
         ON CONFLICT (signal_name) DO UPDATE SET created_at_ms = EXCLUDED.created_at_ms",
    )
    .bind(signal_name)
    .execute(pool)
    .await
    .with_context(|| format!("record harness signal `{signal_name}`"))?;
    Ok(())
}

async fn wait_for_harness_signal(pool: &sqlx::PgPool, signal_name: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let seen: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM lash_e2e_harness_signals WHERE signal_name = $1)",
        )
        .bind(signal_name)
        .fetch_one(pool)
        .await
        .with_context(|| format!("poll harness signal `{signal_name}`"))?;
        if seen {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("timed out waiting for harness signal `{signal_name}`")
}

async fn wait_for_restate_recovery(admin_url: &str) -> Result<()> {
    let admin = RestateAdminClient::new(admin_url.to_string());
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if admin
            .unfinished_invocations_for_service_prefixes(&[TURN_WORKFLOW_NAME])
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("Restate admin API did not recover after engine restart")
}

fn assert_engine_restart_cancelled_terminal(
    terminal: &TurnTerminal,
    request_id: &str,
) -> Result<()> {
    let TurnTerminal::Committed {
        outcome,
        cancellation: Some(evidence),
        ..
    } = terminal
    else {
        anyhow::bail!("expected committed post-restart cancellation, got {terminal:?}")
    };
    anyhow::ensure!(
        matches!(outcome, TurnOutcome::Stopped(TurnStop::Cancelled))
            && evidence.request_id == request_id
            && evidence.origin.as_deref() == Some("scripted-engine-restart-runner")
            && evidence.reason.as_deref()
                == Some("cancel a replayed turn after the Restate engine restarted"),
        "post-restart terminal lost cancellation evidence: {terminal:?}"
    );
    Ok(())
}

async fn drive_break_glass_scenario(
    storage: &PostgresStorage,
    ingress_url: &str,
    admin_url: &str,
) -> Result<()> {
    // Run this last: a hard-killed handler cannot release its shared-session
    // execution lease, and no subsequent scenario should depend on that lease.
    // This remains a negative operator gate and must not manufacture a Lash
    // Cancelled terminal. Graceful Restate cancellation cannot interrupt
    // arbitrary local user code blocked inside a running side-effect closure.
    let break_glass = turn_control_request("e2e-turn-break-glass", false);
    let invocation_id = submit_workflow(ingress_url, &break_glass).await?;
    wait_for_cancel_gate(storage.pool(), &break_glass.workflow_id).await?;
    let admin = RestateAdminClient::new(admin_url.to_string());
    admin
        .kill_invocation_for_test_cleanup(&invocation_id)
        .await
        .context("kill Restate invocation as break-glass")?;
    report_workflow_progress(&break_glass.workflow_id, "admin-kill-requested");
    wait_for_invocation_terminal(&admin, &invocation_id).await?;

    let driver = TurnWorkDriver::new(Arc::new(RestateEffectHost::new(ingress_url.to_string())));
    if let Ok(Ok(terminal)) = tokio::time::timeout(
        Duration::from_secs(3),
        driver.await_terminal(&turn_address(storage, &break_glass).await?),
    )
    .await
    {
        anyhow::ensure!(
            !matches!(
                terminal,
                TurnTerminal::Committed {
                    outcome: TurnOutcome::Stopped(TurnStop::Cancelled),
                    ..
                }
            ),
            "break-glass invocation kill was reported as Lash cancellation"
        );
    }
    anyhow::ensure!(
        load_terminal_result(storage.pool(), &break_glass.workflow_id)
            .await?
            .is_none(),
        "break-glass invocation kill was reported as a Lash terminal result"
    );
    println!("break-glass gate passed: Restate hard-kill was not reported as Lash cancellation");
    Ok(())
}

fn turn_control_request(workflow_id: &str, fail_once: bool) -> TurnRequest {
    TurnRequest {
        workflow_id: workflow_id.to_string(),
        fail_once,
        scenario: TurnScenario::TurnControlHold,
        signal: None,
    }
}

async fn turn_address(_storage: &PostgresStorage, request: &TurnRequest) -> Result<TurnAddress> {
    let session_id = turn_session_id(&request.workflow_id);
    Ok(TurnAddress::new(session_id, request.workflow_id.clone()))
}

fn cancel_request(address: TurnAddress, request_id: &str) -> TurnCancelRequest {
    TurnCancelRequest::new(address, request_id, Some("scripted-e2e-runner".to_string()))
        .with_reason("deterministic Restate/Postgres workers E2E gate")
}

fn assert_requested(outcome: &TurnCancelOutcome, request_id: &str) -> Result<()> {
    let evidence = match outcome {
        TurnCancelOutcome::Requested(evidence) => evidence,
        other => anyhow::bail!("expected cancellation request `{request_id}` to win: {other:?}"),
    };
    anyhow::ensure!(
        evidence.request_id == request_id,
        "cancellation receipt lost request evidence: {evidence:?}"
    );
    anyhow::ensure!(
        evidence.origin.as_deref() == Some("scripted-e2e-runner"),
        "cancellation receipt changed opaque host origin: {evidence:?}"
    );
    Ok(())
}

fn assert_cancelled_terminal(terminal: &TurnTerminal, request_id: &str) -> Result<()> {
    let TurnTerminal::Committed {
        outcome,
        cancellation,
        session_revision: _,
    } = terminal
    else {
        anyhow::bail!("expected committed cancellation terminal, got {terminal:?}")
    };
    anyhow::ensure!(
        matches!(outcome, TurnOutcome::Stopped(TurnStop::Cancelled)),
        "terminal was not Cancelled: {terminal:?}"
    );
    let evidence = cancellation
        .as_ref()
        .context("Cancelled terminal omitted cancellation evidence")?;
    anyhow::ensure!(
        evidence.request_id == request_id,
        "terminal evidence mismatch: {evidence:?}"
    );
    anyhow::ensure!(
        evidence.origin.as_deref() == Some("scripted-e2e-runner"),
        "terminal changed opaque host origin: {evidence:?}"
    );
    Ok(())
}

fn assert_non_cancel_terminal(terminal: &TurnTerminal) -> Result<()> {
    let TurnTerminal::Committed {
        outcome,
        cancellation,
        ..
    } = terminal
    else {
        anyhow::bail!("expected committed completion terminal, got {terminal:?}")
    };
    anyhow::ensure!(
        !matches!(outcome, TurnOutcome::Stopped(TurnStop::Cancelled)),
        "completion-sealed terminal reported Cancelled"
    );
    anyhow::ensure!(
        cancellation.is_none(),
        "non-cancel terminal carried cancellation evidence"
    );
    Ok(())
}

fn assert_cancelled_response(response: &TurnResponse, request_id: &str) -> Result<()> {
    anyhow::ensure!(
        response.final_text == "turn-control-cancelled",
        "worker did not record authoritative cancellation: {response:#?}"
    );
    anyhow::ensure!(
        response.final_value["cancellation"]["request_id"] == request_id,
        "recorded terminal lost cancellation evidence: {response:#?}"
    );
    anyhow::ensure!(
        response.final_value["cancellation"]["origin"] == "scripted-e2e-runner",
        "recorded terminal changed opaque host origin: {response:#?}"
    );
    Ok(())
}

async fn wait_for_cancel_gate(pool: &sqlx::PgPool, workflow_id: &str) -> Result<()> {
    wait_for_cancel_gate_attempts(pool, workflow_id, 1).await
}

async fn wait_for_cancel_gate_attempts(
    pool: &sqlx::PgPool,
    workflow_id: &str,
    expected: i64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lash_e2e_tool_events
             WHERE workflow_id = $1 AND tool_name = 'cancel_gate'",
        )
        .bind(workflow_id)
        .fetch_one(pool)
        .await
        .context("poll cancellation tool gate")?;
        if count >= expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("timed out waiting for {expected} cancellation-gate attempts in `{workflow_id}`")
}

async fn wait_for_failover_marker(pool: &sqlx::PgPool, workflow_id: &str) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let worker = sqlx::query_scalar::<_, String>(
            "SELECT worker_id FROM lash_e2e_failover_markers WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_optional(pool)
        .await
        .context("poll turn-control failover marker")?;
        if let Some(worker) = worker {
            return Ok(worker);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("timed out waiting for owner crash in `{workflow_id}`")
}

async fn wait_for_invocation_terminal(
    admin: &RestateAdminClient,
    invocation_id: &RestateInvocationId,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if admin
            .invocation_status(invocation_id)
            .await
            .context("read break-glass invocation status")?
            .is_some_and(|status| !status.is_still_active())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("break-glass invocation `{invocation_id}` did not terminate")
}

async fn wait_for_invocation_suspended(
    admin: &RestateAdminClient,
    invocation_id: &RestateInvocationId,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let last_status = admin
            .invocation_status(invocation_id)
            .await
            .context("read Restate invocation suspension status")?;
        if last_status
            .as_ref()
            .is_some_and(|status| status.status == "suspended")
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "Restate invocation `{invocation_id}` did not suspend within {timeout:?}; last status={last_status:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `RestateEffectHost::{cancel,revoke}_await_events_for_session` controller
/// route. Restate rejects no-input handler invocations that carry a body or
/// content-type, so this doubles as live regression coverage for the
/// empty-body ingress encoding in `update_restate_session_waits_via_ingress`.
async fn drive_durable_wait_index_scenarios(ingress_url: &str, admin_url: &str) -> Result<()> {
    let host = RestateEffectHost::new(ingress_url.to_string());
    // 1) A controller-owned wait registers in the real Restate session index
    //    and observes cancel_all as a terminal cancellation.
    let cancel_key = host
        .await_event_key(
            &ExecutionScope::turn(DEFAULT_SESSION_ID, "e2e-wait-cancel"),
            AwaitEventWaitIdentity::Custom {
                key: "controller-wait".to_string(),
            },
        )
        .await
        .context("build controller cancellation wait key")?;
    let wait_host = host.clone();
    let wait_key = cancel_key.clone();
    let cancelled_wait = tokio::spawn(async move {
        wait_host
            .await_await_event(
                &wait_key,
                tokio_util::sync::CancellationToken::new(),
                Some(Instant::now() + Duration::from_secs(90)),
            )
            .await
    });
    let deadline = Instant::now() + Duration::from_secs(90);
    while !cancelled_wait.is_finished() {
        host.cancel_await_events_for_session(DEFAULT_SESSION_ID)
            .await
            .context("cancel controller-owned session waits")?;
        anyhow::ensure!(
            Instant::now() < deadline,
            "controller-owned session wait did not observe cancel_all"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let cancelled = cancelled_wait
        .await
        .context("join cancelled controller wait")?
        .context("await cancelled controller wait")?;
    anyhow::ensure!(
        cancelled == Resolution::Cancelled,
        "controller-owned wait resolved unexpectedly: {cancelled:?}"
    );
    assert_no_problem_lash_restate_invocations(admin_url).await?;

    // 2) A new controller-owned wait on the same session resolves normally,
    //    proving cancellation did not permanently revoke the index.
    let reregister_key = host
        .await_event_key(
            &ExecutionScope::turn(DEFAULT_SESSION_ID, "e2e-wait-reregister"),
            AwaitEventWaitIdentity::Custom {
                key: "controller-wait".to_string(),
            },
        )
        .await
        .context("build re-registered controller wait key")?;
    let expected = Resolution::Ok(json!({
        "cancelled": false,
        "answer": "post-cancel-approved",
        "worker_id": "runner"
    }));
    let reregister_resolve = host
        .resolve_await_event(&reregister_key, expected.clone())
        .await
        .context("resolve re-registered durable wait")?;
    anyhow::ensure!(
        matches!(reregister_resolve, lash_core::ResolveOutcome::Accepted),
        "re-registered durable wait resolve was not accepted: {reregister_resolve:?}"
    );
    let reregistered = host
        .await_await_event(
            &reregister_key,
            tokio_util::sync::CancellationToken::new(),
            Some(Instant::now() + Duration::from_secs(90)),
        )
        .await
        .context("await re-registered controller wait")?;
    anyhow::ensure!(
        reregistered == expected,
        "re-registered controller wait resolved unexpectedly: {reregistered:?}"
    );

    // 3) Revoke (the session-deletion path) now includes reserved turn
    // control promises. Validate that contract directly: after revocation,
    // even a duplicate request for an already-created exact turn gate must be
    // rejected by the session index rather than reaching cached workflow
    // state. A containing turn cannot honestly commit after its session has
    // been deleted, so the old post-revoke turn-result assertion no longer
    // applies.
    let control_driver = TurnWorkDriver::new(Arc::new(host.clone()));
    let control_address = TurnAddress::new(DEFAULT_SESSION_ID, "e2e-control-revoke");
    let initial = control_driver
        .request_cancel(TurnCancelRequest::new(
            control_address.clone(),
            "e2e-control-before-revoke",
            Some("scripted-e2e-runner".to_string()),
        ))
        .await
        .context("create turn control gate before revoke")?;
    anyhow::ensure!(
        matches!(initial.outcome, TurnCancelOutcome::Requested(_)),
        "initial turn cancellation was not accepted: {initial:?}"
    );
    host.revoke_await_events_for_session(DEFAULT_SESSION_ID)
        .await
        .context("revoke session turn control promises")?;
    let revoked = control_driver
        .request_cancel(TurnCancelRequest::new(
            control_address,
            "e2e-control-after-revoke",
            Some("scripted-e2e-runner".to_string()),
        ))
        .await
        .context("request cancellation after revoke")?;
    anyhow::ensure!(
        matches!(revoked.outcome, TurnCancelOutcome::UnknownOrRevoked),
        "revoked turn control gate remained addressable: {revoked:?}"
    );
    assert_no_problem_lash_restate_invocations(admin_url).await?;
    eprintln!("durable-wait index gates passed: cancel; reregister; revoke");
    Ok(())
}

async fn wait_for_queued_work(
    storage: &PostgresStorage,
    mock_provider_base_url: &str,
    trace_dir: Option<PathBuf>,
    ingress_url: &str,
) -> Result<()> {
    let registry = process_registry_from_storage(storage);
    let deployment = RestateProcessDeployment::new(ingress_url.to_string(), registry);
    let process_work_driver = deployment.process_work_driver();
    let core = build_e2e_core(lash_restate_postgres_workers_e2e::E2eCoreConfig {
        worker_id: "runner-queue-watch".to_string(),
        storage: storage.clone(),
        attachment_store: Arc::new(s3_store_from_env()?)
            as Arc<dyn lash::persistence::AttachmentStore>,
        process_work_driver,
        restate_ingress_url: ingress_url.to_string(),
        mock_provider_base_url: mock_provider_base_url.to_string(),
        trace_dir,
        fail_once: false,
    })?;
    let session = core.session(DEFAULT_SESSION_ID).open().await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let queued = session.queued_work().await?;
        if !queued.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("timed out waiting for queued process wake")
}

async fn wait_for_process_signal_wait(
    pool: &sqlx::PgPool,
    process_id: &str,
    signal_name: &str,
    ordinal: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT status, record_json FROM lash_processes WHERE process_id = $1")
                .bind(process_id)
                .fetch_optional(pool)
                .await
                .with_context(|| format!("load process `{process_id}` wait state"))?;
        if let Some((status, record_json)) = row {
            anyhow::ensure!(
                status == "running",
                "process `{process_id}` reached status `{status}` before signal `{signal_name}` wait"
            );
            let record: Value = serde_json::from_str(&record_json)
                .with_context(|| format!("decode process `{process_id}` record"))?;
            let wait = record.get("wait").cloned().unwrap_or(Value::Null);
            let kind = wait.get("kind").cloned().unwrap_or(Value::Null);
            if kind.get("kind").and_then(Value::as_str) == Some("signal")
                && kind.get("name").and_then(Value::as_str) == Some(signal_name)
                && kind.get("ordinal").and_then(Value::as_u64) == Some(ordinal)
            {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("timed out waiting for process `{process_id}` signal `{signal_name}` wait")
}

async fn emit_button_event(
    storage: &PostgresStorage,
    mock_provider_base_url: &str,
    trace_dir: Option<PathBuf>,
    ingress_url: &str,
) -> Result<String> {
    let registry = process_registry_from_storage(storage);
    let deployment = RestateProcessDeployment::new(ingress_url.to_string(), registry);
    let process_work_driver = deployment.process_work_driver();
    let core = build_e2e_core(lash_restate_postgres_workers_e2e::E2eCoreConfig {
        worker_id: "runner".to_string(),
        storage: storage.clone(),
        attachment_store: Arc::new(s3_store_from_env()?)
            as Arc<dyn lash::persistence::AttachmentStore>,
        process_work_driver,
        restate_ingress_url: ingress_url.to_string(),
        mock_provider_base_url: mock_provider_base_url.to_string(),
        trace_dir,
        fail_once: false,
    })?;
    let source_key = empty_trigger_source_key(BUTTON_SOURCE_TYPE)?;
    let scoped = ScopedEffectController::shared(
        Arc::new(InlineRuntimeEffectController::default()),
        ExecutionScope::runtime_operation("e2e-button-trigger"),
    )?;
    let report = core
        .triggers()
        .emit(
            TriggerOccurrenceRequest::new(
                BUTTON_SOURCE_TYPE,
                source_key,
                json!({
                    "button": "Red",
                    "message": "pressed from runner",
                    "pressed_at": "2026-06-08T12:00:00Z"
                }),
                "e2e-button-red-1",
            )
            .with_source(json!({"runner": true})),
            scoped,
        )
        .await?;
    report
        .started_process_ids()
        .first()
        .cloned()
        .context("trigger occurrence did not start a process")
}

async fn assert_signal_process_output(pool: &sqlx::PgPool, process_id: &str) -> Result<()> {
    let event_json: String = sqlx::query_scalar(
        "SELECT event_json
         FROM lash_process_events
         WHERE process_id = $1 AND event_type = 'process.completed'",
    )
    .bind(process_id)
    .fetch_one(pool)
    .await
    .with_context(|| format!("load completed event for signal process `{process_id}`"))?;
    let event: Value = serde_json::from_str(&event_json)
        .with_context(|| format!("decode completed event for signal process `{process_id}`"))?;
    let await_output = event
        .pointer("/payload/await_output")
        .with_context(|| format!("completed event missing await output: {event}"))?;
    anyhow::ensure!(
        await_output.get("type").and_then(Value::as_str) == Some("success"),
        "signal process `{process_id}` completed with non-success output: {await_output}"
    );
    let value = await_output
        .get("value")
        .with_context(|| format!("completed event missing success value: {event}"))?;
    anyhow::ensure!(
        value.pointer("/first/phase").and_then(Value::as_str) == Some("first")
            && value.pointer("/second/phase").and_then(Value::as_str) == Some("second"),
        "signal process `{process_id}` completed with unexpected value: {value}"
    );

    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event_type
         FROM lash_process_events
         WHERE process_id = $1
         ORDER BY sequence",
    )
    .bind(process_id)
    .fetch_all(pool)
    .await
    .with_context(|| format!("load signal process `{process_id}` events"))?;
    let first_signal = events
        .iter()
        .position(|event| event == "signal.first")
        .context("signal.first event missing")?;
    let second_signal = events
        .iter()
        .position(|event| event == "signal.second")
        .context("signal.second event missing")?;
    let completed = events
        .iter()
        .position(|event| event == "process.completed")
        .context("process.completed event missing")?;
    anyhow::ensure!(
        first_signal < second_signal && second_signal < completed,
        "signal process events out of order: {events:?}"
    );
    Ok(())
}

async fn wait_for_process_terminal(pool: &sqlx::PgPool, process_id: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM lash_processes WHERE process_id = $1")
                .bind(process_id)
                .fetch_optional(pool)
                .await
                .with_context(|| format!("load process `{process_id}` status"))?;
        if matches!(
            status.as_deref(),
            Some("completed" | "failed" | "cancelled")
        ) {
            anyhow::ensure!(
                status.as_deref() == Some("completed"),
                "trigger process `{process_id}` ended with {status:?}"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("timed out waiting for process `{process_id}`")
}

async fn assert_processes_terminal(pool: &sqlx::PgPool) -> Result<()> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT process_id, status, record_json
         FROM lash_processes
         ORDER BY created_at_ms, process_id",
    )
    .fetch_all(pool)
    .await
    .context("load process rows")?;
    anyhow::ensure!(
        rows.len() >= 11,
        "expected at least 11 process rows for kitchen sink + failover + trigger + signal + async completion, got {}",
        rows.len()
    );
    let terminal = rows
        .iter()
        .filter(|(_, status, _)| matches!(status.as_str(), "completed" | "failed" | "cancelled"))
        .count();
    anyhow::ensure!(
        terminal == rows.len(),
        "expected all process rows terminal, got {terminal}/{}: {rows:?}",
        rows.len()
    );
    let record_text = rows
        .iter()
        .map(|(_, _, record)| record.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "async_child",
        "async:detached",
        "parent",
        "child",
        "on_button",
        "lookup:left",
        "lookup:right",
    ] {
        anyhow::ensure!(
            record_text.contains(needle),
            "process records did not contain `{needle}`"
        );
    }

    let inconsistent_terminal_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
            SELECT p.process_id
            FROM lash_processes p
            LEFT JOIN lash_process_events e
              ON e.process_id = p.process_id
             AND e.event_type IN ('process.completed', 'process.failed', 'process.cancelled')
            GROUP BY p.process_id
            HAVING COUNT(e.process_id) <> 1
        ) inconsistent",
    )
    .fetch_one(pool)
    .await
    .context("count process rows without exactly one terminal event")?;
    anyhow::ensure!(
        inconsistent_terminal_events == 0,
        "expected every process to have exactly one terminal event"
    );
    Ok(())
}

async fn assert_no_duplicate_runtime_rows(pool: &sqlx::PgPool) -> Result<()> {
    let queued_work_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lash_queued_work_batches WHERE session_id = $1")
            .bind(DEFAULT_SESSION_ID)
            .fetch_one(pool)
            .await
            .context("count queued work rows")?;
    anyhow::ensure!(
        queued_work_count == 0,
        "expected no leftover queued work rows after wake consumption, got {queued_work_count}"
    );
    let duplicate_turn_commits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
            SELECT session_id, turn_id
            FROM lash_runtime_turn_commits
            WHERE session_id = $1
            GROUP BY session_id, turn_id
            HAVING COUNT(*) > 1
        ) duplicates",
    )
    .bind(DEFAULT_SESSION_ID)
    .fetch_one(pool)
    .await
    .context("count duplicate runtime turn commits")?;
    anyhow::ensure!(
        duplicate_turn_commits == 0,
        "duplicate runtime turn commits were recorded"
    );
    let artifacts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lash_lashlang_artifacts")
        .fetch_one(pool)
        .await
        .context("count Lashlang artifacts")?;
    anyhow::ensure!(artifacts > 0, "expected Lashlang artifact rows");
    Ok(())
}

async fn assert_worker_distribution(pool: &sqlx::PgPool) -> Result<()> {
    let workers: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT worker_id FROM lash_e2e_worker_events ORDER BY worker_id",
    )
    .fetch_all(pool)
    .await
    .context("list worker ids")?;
    let workers = workers.into_iter().collect::<BTreeSet<_>>();
    anyhow::ensure!(
        workers.contains("worker-a") && workers.contains("worker-b"),
        "expected both worker-a and worker-b to handle work, got {workers:?}"
    );
    Ok(())
}

async fn assert_failover(pool: &sqlx::PgPool) -> Result<()> {
    for workflow_id in [
        "e2e-failover",
        "e2e-tool-batch-failover",
        "e2e-process-llm-query-replay",
    ] {
        // The crash injector keeps the marker's logical worker unavailable for
        // this workflow even after Compose restarts its container. Completion
        // therefore proves takeover by the healthy peer rather than depending
        // on whether the crashed container wins the restart/retry race.
        let exit_worker: String = sqlx::query_scalar(
            "SELECT worker_id
             FROM lash_e2e_failover_markers
             WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_one(pool)
        .await
        .with_context(|| format!("load failover exit marker for `{workflow_id}`"))?;
        let completed_by: Vec<String> = sqlx::query_scalar(
            "SELECT worker_id
             FROM lash_e2e_worker_events
             WHERE workflow_id = $1
               AND event_type = 'turn_completed'
             ORDER BY worker_id",
        )
        .bind(workflow_id)
        .fetch_all(pool)
        .await
        .with_context(|| format!("load failover completion worker for `{workflow_id}`"))?;
        anyhow::ensure!(
            completed_by.iter().any(|worker| worker != &exit_worker),
            "expected a peer to complete `{workflow_id}` after {exit_worker} exited, got {completed_by:?}"
        );
        let final_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM lash_e2e_terminal_results
             WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_one(pool)
        .await
        .with_context(|| format!("count failover final rows for `{workflow_id}`"))?;
        anyhow::ensure!(
            final_rows == 1,
            "failover workflow `{workflow_id}` recorded {final_rows} final rows"
        );
    }
    Ok(())
}

async fn assert_provider_calls(pool: &sqlx::PgPool) -> Result<()> {
    let bad_model: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_e2e_provider_calls WHERE model <> 'e2e-mock'",
    )
    .fetch_one(pool)
    .await
    .context("count provider calls with wrong model")?;
    anyhow::ensure!(bad_model == 0, "provider saw {bad_model} wrong-model calls");
    let failover_calls: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_e2e_provider_calls
         WHERE workflow_id = 'e2e-failover' AND scenario = 'kitchen_sink'",
    )
    .fetch_one(pool)
    .await
    .context("count failover provider calls")?;
    anyhow::ensure!(
        failover_calls == 1,
        "expected one durable failover provider completion, got {failover_calls}"
    );
    let tool_batch_failover_calls: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_e2e_provider_calls
         WHERE workflow_id = 'e2e-tool-batch-failover' AND scenario = 'tool_batch'",
    )
    .fetch_one(pool)
    .await
    .context("count tool-batch failover provider calls")?;
    anyhow::ensure!(
        tool_batch_failover_calls == 1,
        "expected one durable tool-batch failover provider completion, got {tool_batch_failover_calls}"
    );
    for workflow_id in ["e2e-process-llm-query", "e2e-process-llm-query-replay"] {
        let direct_calls: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lash_e2e_provider_calls
             WHERE workflow_id = $1 AND scenario = 'process_llm_query_direct'",
        )
        .bind(workflow_id)
        .fetch_one(pool)
        .await
        .with_context(|| format!("count process llm_query direct calls for `{workflow_id}`"))?;
        anyhow::ensure!(
            direct_calls == 1,
            "workflow `{workflow_id}` invoked the llm_query provider {direct_calls} times; completed-attempt replay must reuse the recorded attempt"
        );
    }
    let scenarios: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT scenario FROM lash_e2e_provider_calls ORDER BY scenario",
    )
    .fetch_all(pool)
    .await
    .context("list provider scenarios")?;
    for expected in [
        "async_completion",
        "durable_input_request",
        "kitchen_sink",
        "parent_durable_input_after_child",
        "process_llm_query",
        "process_llm_query_direct",
        "queued_wake",
        "trigger_setup",
        "signal_suspend",
        "tool_batch",
    ] {
        anyhow::ensure!(
            scenarios.iter().any(|scenario| scenario == expected),
            "provider scenario `{expected}` missing from {scenarios:?}"
        );
    }
    Ok(())
}

async fn assert_tool_and_turn_telemetry(pool: &sqlx::PgPool) -> Result<()> {
    for tool in [
        "app_lookup",
        "async_lookup",
        "batch_side_effect",
        "make_attachment",
        "crash_once",
        "durable_input_request.opened",
    ] {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM lash_e2e_tool_events WHERE tool_name = $1")
                .bind(tool)
                .fetch_one(pool)
                .await
                .with_context(|| format!("count tool events for `{tool}`"))?;
        anyhow::ensure!(count > 0, "missing tool telemetry for `{tool}`");
    }
    let async_resolutions: Vec<String> = sqlx::query_scalar(
        "SELECT result_json
         FROM lash_e2e_tool_events
         WHERE tool_name = 'async_lookup.resolve'",
    )
    .fetch_all(pool)
    .await
    .context("load async lookup resolution telemetry")?;
    let accepted = async_resolutions
        .iter()
        .filter_map(|row| serde_json::from_str::<Value>(row).ok())
        .any(|row| {
            row.pointer("/outcome/status").and_then(Value::as_str) == Some("accepted")
                && row.pointer("/result/value").and_then(Value::as_str) == Some("async:detached")
        });
    anyhow::ensure!(
        accepted,
        "async lookup did not record an accepted external resolution: {async_resolutions:?}"
    );
    let turn_events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lash_e2e_turn_events")
        .fetch_one(pool)
        .await
        .context("count streamed turn events")?;
    anyhow::ensure!(turn_events > 0, "no streamed turn activities were recorded");
    let cursor_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_e2e_turn_events
         WHERE stream_name = 'main' AND cursor IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("count cursor-bearing turn events")?;
    anyhow::ensure!(
        cursor_events > 0,
        "no streamed turn event recorded a replay cursor"
    );
    let live_replay_checks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_e2e_worker_events WHERE event_type = 'live_replay_checked'",
    )
    .fetch_one(pool)
    .await
    .context("count live replay checks")?;
    anyhow::ensure!(
        live_replay_checks >= 5,
        "expected live replay checks for five workflow turns, got {live_replay_checks}"
    );
    Ok(())
}

async fn assert_durable_input_attempts(pool: &sqlx::PgPool) -> Result<()> {
    for workflow_id in ["e2e-durable-input", "e2e-parent-durable-input-after-child"] {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT step_id, count
             FROM lash_e2e_tool_attempt_counts
             WHERE workflow_id = $1
             ORDER BY step_id",
        )
        .bind(workflow_id)
        .fetch_all(pool)
        .await
        .with_context(|| format!("load durable input attempt counts for `{workflow_id}`"))?;
        let counts = rows
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        let count = counts.get("attempt").copied().unwrap_or_default();
        anyhow::ensure!(
            count == 1,
            "workflow `{workflow_id}` durable input attempt ran {count} times; counts={counts:?}"
        );
    }
    Ok(())
}

async fn assert_tool_batch_side_effects(pool: &sqlx::PgPool) -> Result<()> {
    for workflow_id in ["e2e-tool-batch", "e2e-tool-batch-failover"] {
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT args_json::jsonb ->> 'key' AS key,
                    COUNT(*) AS count,
                    COUNT(DISTINCT call_id) AS distinct_call_ids
             FROM lash_e2e_tool_events
             WHERE workflow_id = $1 AND tool_name = 'batch_side_effect'
             GROUP BY args_json::jsonb ->> 'key'
             ORDER BY key",
        )
        .bind(workflow_id)
        .fetch_all(pool)
        .await
        .with_context(|| format!("load tool-batch side effects for `{workflow_id}`"))?;
        let counts = rows
            .into_iter()
            .map(|(key, count, distinct_call_ids)| (key, (count, distinct_call_ids)))
            .collect::<std::collections::BTreeMap<_, _>>();
        for key in ["fast", "slow"] {
            let (count, distinct_call_ids) = counts.get(key).copied().unwrap_or_default();
            anyhow::ensure!(
                count == 1,
                "workflow `{workflow_id}` recorded {count} side effects for `{key}`; counts={counts:?}"
            );
            anyhow::ensure!(
                distinct_call_ids == 1,
                "workflow `{workflow_id}` recorded {distinct_call_ids} call ids for `{key}`; counts={counts:?}"
            );
        }
        anyhow::ensure!(
            counts.len() == 2,
            "workflow `{workflow_id}` recorded unexpected tool-batch side-effect keys: {counts:?}"
        );
    }
    Ok(())
}

async fn assert_trigger_delivery(pool: &sqlx::PgPool, trigger_process_id: &str) -> Result<()> {
    let trigger_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_trigger_subscriptions
         WHERE source_type = $1 AND enabled = true",
    )
    .bind(BUTTON_SOURCE_TYPE)
    .fetch_one(pool)
    .await
    .context("count trigger subscriptions")?;
    anyhow::ensure!(
        trigger_count == 1,
        "expected one enabled trigger, got {trigger_count}"
    );
    let delivery_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lash_trigger_deliveries WHERE process_id = $1")
            .bind(trigger_process_id)
            .fetch_one(pool)
            .await
            .context("count trigger occurrence deliveries")?;
    anyhow::ensure!(
        delivery_count == 1,
        "expected one trigger delivery for `{trigger_process_id}`, got {delivery_count}"
    );
    Ok(())
}

async fn assert_attachments_round_trip(
    pool: &sqlx::PgPool,
    store: &impl lash::persistence::AttachmentStore,
    responses: &[TurnResponse],
) -> Result<()> {
    for response in responses
        .iter()
        .filter(|response| !response.attachment_id.is_empty())
    {
        let id = lash_core::AttachmentId::new(response.attachment_id.clone());
        let manifest: Option<(String, Option<i64>)> = sqlx::query_as(
            "SELECT session_id, committed_at_ms
             FROM lash_attachment_manifest
             WHERE attachment_id = $1",
        )
        .bind(response.attachment_id.as_str())
        .fetch_optional(pool)
        .await
        .with_context(|| format!("load attachment manifest for `{}`", response.attachment_id))?;
        let (session_id, committed_at_ms) = manifest.with_context(|| {
            format!(
                "missing attachment manifest row for `{}`",
                response.attachment_id
            )
        })?;
        // Blob storage is flat and content-addressed now; session ownership is
        // asserted through the Postgres manifest row below, not the object key.
        let stored = store
            .get(&id)
            .await
            .with_context(|| format!("read worker attachment `{id}` from MinIO"))?;
        anyhow::ensure!(
            stored.bytes == expected_attachment_bytes(&response.workflow_id),
            "worker attachment `{id}` bytes did not match expected content"
        );
        anyhow::ensure!(
            session_id == DEFAULT_SESSION_ID,
            "attachment manifest session mismatch for `{}`: {session_id}",
            response.attachment_id
        );
        anyhow::ensure!(
            committed_at_ms.is_some(),
            "attachment manifest row for `{}` was not committed",
            response.attachment_id
        );
    }
    Ok(())
}

async fn assert_reopened_session_agrees(
    storage: &PostgresStorage,
    mock_provider_base_url: &str,
    trace_dir: Option<PathBuf>,
    ingress_url: &str,
    responses: &[TurnResponse],
) -> Result<()> {
    let registry = process_registry_from_storage(storage);
    let deployment = RestateProcessDeployment::new(ingress_url.to_string(), registry);
    let process_work_driver = deployment.process_work_driver();
    let core = build_e2e_core(lash_restate_postgres_workers_e2e::E2eCoreConfig {
        worker_id: "runner-reopen".to_string(),
        storage: storage.clone(),
        attachment_store: Arc::new(s3_store_from_env()?)
            as Arc<dyn lash::persistence::AttachmentStore>,
        process_work_driver,
        restate_ingress_url: ingress_url.to_string(),
        mock_provider_base_url: mock_provider_base_url.to_string(),
        trace_dir,
        fail_once: false,
    })?;
    let session = core.session(DEFAULT_SESSION_ID).open().await?;
    let read = storage
        .session_store(DEFAULT_SESSION_ID)
        .load_session()
        .await
        .context("load persisted runtime session")?
        .context("runtime session was not persisted")?;
    anyhow::ensure!(
        read.session_id == DEFAULT_SESSION_ID,
        "expected session `{}`, got `{}`",
        DEFAULT_SESSION_ID,
        read.session_id
    );
    let queued = session.queued_work().await?;
    anyhow::ensure!(
        queued.is_empty(),
        "reopened session had queued work: {queued:?}"
    );
    let submitted_finals = responses
        .iter()
        .filter_map(|response| {
            response
                .final_value
                .get("final")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        submitted_finals.contains(EXPECTED_FINAL_TEXT),
        "reopened assertion did not see submitted final `{EXPECTED_FINAL_TEXT}` in {submitted_finals:?}"
    );
    Ok(())
}

async fn assert_traces(trace_dir: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let files = std::fs::read_dir(trace_dir)
            .with_context(|| format!("read trace dir `{}`", trace_dir.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect::<Vec<_>>();
        if !files.is_empty() {
            let mut combined = String::new();
            for file in files {
                combined.push_str(&std::fs::read_to_string(&file).unwrap_or_default());
                combined.push('\n');
            }
            for needle in [
                "app_lookup",
                "async_lookup",
                "make_attachment",
                "crash_once",
                "parent",
                "child",
                "parent_wake",
                "on_button",
            ] {
                anyhow::ensure!(
                    combined.contains(needle),
                    "trace JSONL did not contain `{needle}`"
                );
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("no trace JSONL files appeared in `{}`", trace_dir.display())
}
