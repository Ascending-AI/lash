use super::*;

/// `RestateEffectHost::{cancel,revoke}_await_events_for_session` controller
/// route. Restate rejects no-input handler invocations that carry a body or
/// content-type, so this doubles as live regression coverage for the
/// empty-body ingress encoding in `update_restate_session_waits_via_ingress`.
pub(super) async fn drive_durable_wait_index_scenarios(
    storage: &PostgresStorage,
    ingress_url: &str,
    admin_url: &str,
) -> Result<()> {
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
        host.cancel_await_events_for_session(&SessionId::from(DEFAULT_SESSION_ID))
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
    let control_driver = TurnWorkDriver::for_catalog(
        Arc::new(host.clone()),
        Arc::new(storage.session_store_factory()),
    );
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
    host.revoke_await_events_for_session(&SessionId::from(DEFAULT_SESSION_ID))
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

pub(super) async fn wait_for_queued_work(
    storage: &PostgresStorage,
    mock_provider_base_url: &str,
    trace_dir: Option<PathBuf>,
    ingress_url: &str,
) -> Result<()> {
    let registry = process_registry_from_storage(storage);
    let continuations =
        lash_restate_postgres_workers_e2e::process_continuations_from_storage(storage);
    let deployment =
        RestateProcessDeployment::new(ingress_url.to_string(), registry, continuations);
    let process_work_driver = deployment.process_work();
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

pub(super) async fn wait_for_process_signal_wait(
    pool: &sqlx::PgPool,
    process_id: &ProcessId,
    signal_name: &str,
    ordinal: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT status, record_json FROM lash_processes WHERE process_id = $1")
                .bind(process_id.as_str())
                .fetch_optional(pool)
                .await
                .with_context(|| format!("load process `{process_id}` wait state"))?;
        if let Some((status, record_json)) = row {
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
            anyhow::ensure!(
                matches!(status.as_str(), "running" | "waiting"),
                "process `{process_id}` reached status `{status}` before signal `{signal_name}` wait"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("timed out waiting for process `{process_id}` signal `{signal_name}` wait")
}

pub(super) async fn emit_button_event(
    storage: &PostgresStorage,
    mock_provider_base_url: &str,
    trace_dir: Option<PathBuf>,
    ingress_url: &str,
) -> Result<ProcessId> {
    let registry = process_registry_from_storage(storage);
    let continuations =
        lash_restate_postgres_workers_e2e::process_continuations_from_storage(storage);
    let deployment =
        RestateProcessDeployment::new(ingress_url.to_string(), registry, continuations);
    let process_work_driver = deployment.process_work();
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
        Arc::new(NativeRuntimeEffectController::default()),
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

pub(super) fn signal_process_output_value(await_output: Value) -> Result<Value> {
    let await_output: lash_core::ProcessAwaitOutput =
        serde_json::from_value(await_output).context("decode typed signal process await output")?;
    let output = await_output.into_tool_output();
    anyhow::ensure!(
        output.is_success(),
        "signal process completed with non-success output: {output:?}"
    );
    Ok(output.value_for_projection())
}

pub(super) async fn assert_signal_process_output(
    pool: &sqlx::PgPool,
    process_id: &ProcessId,
) -> Result<()> {
    let event_json: String = sqlx::query_scalar(
        "SELECT event_json
         FROM lash_process_events
         WHERE process_id = $1 AND event_type = 'process.completed'",
    )
    .bind(process_id.as_str())
    .fetch_one(pool)
    .await
    .with_context(|| format!("load completed event for signal process `{process_id}`"))?;
    let event: Value = serde_json::from_str(&event_json)
        .with_context(|| format!("decode completed event for signal process `{process_id}`"))?;
    let await_output = event
        .pointer("/payload/await_output")
        .with_context(|| format!("completed event missing await output: {event}"))?;
    let value = signal_process_output_value(await_output.clone())
        .with_context(|| format!("decode signal process output: {await_output}"))?;
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
    .bind(process_id.as_str())
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

pub(super) async fn wait_for_process_terminal(
    pool: &sqlx::PgPool,
    process_id: &ProcessId,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM lash_processes WHERE process_id = $1")
                .bind(process_id.as_str())
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

pub(super) async fn assert_processes_terminal(pool: &sqlx::PgPool) -> Result<()> {
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

pub(super) async fn assert_no_duplicate_runtime_rows(pool: &sqlx::PgPool) -> Result<()> {
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

pub(super) async fn assert_worker_distribution(pool: &sqlx::PgPool) -> Result<()> {
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

pub(super) async fn assert_failover(
    pool: &sqlx::PgPool,
    selection: SegmentSelection,
) -> Result<()> {
    let workflow_ids = match selection {
        SegmentSelection::All => &[
            "e2e-failover",
            "e2e-tool-batch-failover",
            "e2e-process-llm-query-replay",
        ][..],
        SegmentSelection::One => &["e2e-failover", "e2e-process-llm-query-replay"][..],
        SegmentSelection::Two => &["e2e-tool-batch-failover"][..],
    };
    for workflow_id in workflow_ids {
        // FIG-1671 cede semantics (ADR 0069 §5(d)): the identity of the worker
        // that finishes a failed-over turn is not the invariant — convergence on
        // the journaled acceptance is. This gate used to require a *peer* to
        // record the completion, on the premise that "the crash injector keeps
        // the marker's logical worker unavailable for this workflow even after
        // Compose restarts its container". That premise is false under journal
        // replay: the peer drives the turn, loses the head CAS to the dead
        // holder's claim, and cedes; the engine's retry then replays the
        // *journaled* `crash_once` call instead of re-invoking it, so the
        // reincarnated original worker never re-exits and routinely finishes the
        // turn. That is exactly the retryable-by-contract behaviour the cede
        // ruling ratified, so the witness is the durable one: exactly one
        // completion, against exactly one acceptance, settled once. The ~38s
        // lease-TTL residual these failovers now pay is settled by ADR 0080:
        // substrate exclusivity is not a lease short-circuit, and shortening the
        // wait is the host's `LeaseTimings` decision (ADR 0014).
        //
        // The crash still has to happen — the marker read below fails the gate
        // if no worker ever exited for this workflow.
        let _exit_worker: String = sqlx::query_scalar(
            "SELECT worker_id
             FROM lash_e2e_failover_markers
             WHERE workflow_id = $1",
        )
        .bind(*workflow_id)
        .fetch_one(pool)
        .await
        .with_context(|| format!("load failover exit marker for `{workflow_id}`"))?;
        assert_recovered_turn_converged(pool, &TurnId::from(*workflow_id)).await?;
        let final_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM lash_e2e_terminal_results
             WHERE workflow_id = $1",
        )
        .bind(*workflow_id)
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

pub(super) async fn assert_provider_calls(
    pool: &sqlx::PgPool,
    selection: SegmentSelection,
) -> Result<()> {
    let bad_model: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_e2e_provider_calls WHERE model <> 'e2e-mock'",
    )
    .fetch_one(pool)
    .await
    .context("count provider calls with wrong model")?;
    anyhow::ensure!(bad_model == 0, "provider saw {bad_model} wrong-model calls");
    if selection.includes(WorkflowSegment::One) {
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
    }
    if selection.includes(WorkflowSegment::Two) {
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
    }
    let scenarios: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT scenario FROM lash_e2e_provider_calls ORDER BY scenario",
    )
    .fetch_all(pool)
    .await
    .context("list provider scenarios")?;
    let mut expected_scenarios = Vec::new();
    if selection.includes(WorkflowSegment::One) {
        expected_scenarios.extend([
            "async_completion",
            "durable_input_request",
            "kitchen_sink",
            "parent_durable_input_after_child",
            "process_llm_query",
            "process_llm_query_direct",
            "queued_wake",
            "trigger_setup",
            "signal_suspend",
        ]);
    }
    if selection.includes(WorkflowSegment::Two) {
        expected_scenarios.push("tool_batch");
    }
    for expected in expected_scenarios {
        anyhow::ensure!(
            scenarios.iter().any(|scenario| scenario == expected),
            "provider scenario `{expected}` missing from {scenarios:?}"
        );
    }
    Ok(())
}

pub(super) async fn assert_tool_and_turn_telemetry(
    pool: &sqlx::PgPool,
    selection: SegmentSelection,
) -> Result<()> {
    let tools = match selection {
        SegmentSelection::All => &[
            "app_lookup",
            "async_lookup",
            "batch_side_effect",
            "make_attachment",
            "crash_once",
            "durable_input_request.opened",
        ][..],
        SegmentSelection::One => &[
            "app_lookup",
            "async_lookup",
            "make_attachment",
            "crash_once",
            "durable_input_request.opened",
        ][..],
        SegmentSelection::Two => &["batch_side_effect", "crash_once"][..],
    };
    for tool in tools {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM lash_e2e_tool_events WHERE tool_name = $1")
                .bind(*tool)
                .fetch_one(pool)
                .await
                .with_context(|| format!("count tool events for `{tool}`"))?;
        anyhow::ensure!(count > 0, "missing tool telemetry for `{tool}`");
    }
    if selection.includes(WorkflowSegment::One) {
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
                    && row.pointer("/result/value").and_then(Value::as_str)
                        == Some("async:detached")
            });
        anyhow::ensure!(
            accepted,
            "async lookup did not record an accepted external resolution: {async_resolutions:?}"
        );
    }
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
    let minimum_live_replay_checks = match selection {
        SegmentSelection::All => 5,
        SegmentSelection::One => 3,
        SegmentSelection::Two => 2,
    };
    anyhow::ensure!(
        live_replay_checks >= minimum_live_replay_checks,
        "expected at least {minimum_live_replay_checks} live replay checks for segment {}, got {live_replay_checks}",
        selection.label()
    );
    Ok(())
}

pub(super) async fn assert_durable_input_attempts(pool: &sqlx::PgPool) -> Result<()> {
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

pub(super) async fn assert_tool_batch_side_effects(pool: &sqlx::PgPool) -> Result<()> {
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

pub(super) async fn assert_trigger_delivery(
    pool: &sqlx::PgPool,
    trigger_process_id: &ProcessId,
) -> Result<()> {
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
            .bind(trigger_process_id.as_str())
            .fetch_one(pool)
            .await
            .context("count trigger occurrence deliveries")?;
    anyhow::ensure!(
        delivery_count == 1,
        "expected one trigger delivery for `{trigger_process_id}`, got {delivery_count}"
    );
    Ok(())
}

pub(super) async fn assert_attachments_round_trip(
    pool: &sqlx::PgPool,
    store: &impl lash::persistence::AttachmentStore,
    responses: &[TurnResponse],
) -> Result<()> {
    for response in responses
        .iter()
        .filter(|response| !response.attachment_id.is_empty())
    {
        let id = lash_core::AttachmentId::parse(&response.attachment_id).with_context(|| {
            format!(
                "workbench returned an unusable attachment id `{}`",
                response.attachment_id
            )
        })?;
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

pub(super) async fn assert_reopened_session_agrees(
    storage: &PostgresStorage,
    mock_provider_base_url: &str,
    trace_dir: Option<PathBuf>,
    ingress_url: &str,
    responses: &[TurnResponse],
) -> Result<()> {
    let registry = process_registry_from_storage(storage);
    let continuations =
        lash_restate_postgres_workers_e2e::process_continuations_from_storage(storage);
    let deployment =
        RestateProcessDeployment::new(ingress_url.to_string(), registry, continuations);
    let process_work_driver = deployment.process_work();
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

pub(super) async fn assert_traces(trace_dir: &Path, selection: SegmentSelection) -> Result<()> {
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
            let needles = match selection {
                SegmentSelection::All | SegmentSelection::One => &[
                    "app_lookup",
                    "async_lookup",
                    "make_attachment",
                    "crash_once",
                    "parent",
                    "child",
                    "parent_wake",
                    "on_button",
                ][..],
                SegmentSelection::Two => &["batch_side_effect", "crash_once"][..],
            };
            for needle in needles {
                anyhow::ensure!(
                    combined.contains(*needle),
                    "trace JSONL did not contain `{needle}`"
                );
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("no trace JSONL files appeared in `{}`", trace_dir.display())
}
