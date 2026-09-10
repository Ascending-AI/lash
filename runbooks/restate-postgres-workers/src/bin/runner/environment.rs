use super::*;

pub(super) async fn run_engine_promise_gates(admin_url: &str, ingress_url: &str) -> Result<()> {
    run_engine_promise_conformance(admin_url, ingress_url).await?;
    println!(
        "durable-wait wake gates passed: waiter-before-resolution; resolution-before-waiter; peer-worker resolution across failover"
    );
    Ok(())
}

pub(super) async fn run_cold_process_await_event_vectors(
    admin_url: &str,
    ingress_url: &str,
) -> Result<()> {
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

pub(super) async fn runner_stall_watchdog(
    pool: sqlx::PgPool,
    admin_url: String,
    timeout: Duration,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let stalled = {
            let progress = runner_progress().lock_recover();
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

pub(super) async fn dump_runner_stall_diagnostics(pool: &sqlx::PgPool, admin_url: &str) {
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

pub(super) async fn dump_workflow_timeout_diagnostics(pool: &sqlx::PgPool, workflow_id: &str) {
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

pub(super) fn reset_trace_dir(dir: &Path) -> Result<()> {
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

pub(super) async fn wait_for_postgres(database_url: &str) -> Result<PostgresStorage> {
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

pub(super) async fn drive_frame_switch_crash_process(
    storage: &PostgresStorage,
) -> Result<TurnResponse> {
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

pub(super) async fn wait_for_minio(store: &impl lash::persistence::AttachmentStore) -> Result<()> {
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

pub(super) async fn register_restate_deployment(
    admin_url: &str,
    deployment_url: &str,
) -> Result<()> {
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

pub(super) async fn wait_for_mock_provider(base_url: &str) -> Result<()> {
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

pub(super) async fn submit_workflow(
    ingress_url: &str,
    request: &TurnRequest,
) -> Result<RestateInvocationId> {
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

pub(super) async fn submit_signal_workflow(
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

pub(super) async fn assert_no_active_lash_restate_invocations(admin_url: &str) -> Result<()> {
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

pub(super) async fn assert_no_problem_lash_restate_invocations(admin_url: &str) -> Result<()> {
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

pub(super) fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(super) async fn wait_for_terminal_result(
    pool: &sqlx::PgPool,
    workflow_id: &str,
) -> Result<TurnResponse> {
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

pub(super) async fn wait_for_terminal_results(
    pool: &sqlx::PgPool,
    expected_workflows: &[&str],
) -> Result<Vec<TurnResponse>> {
    let expected = expected_workflows.iter().copied().collect::<BTreeSet<_>>();
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
        let actual = rows
            .iter()
            .map(|row| row.0.as_str())
            .collect::<BTreeSet<_>>();
        if actual == expected {
            return rows
                .into_iter()
                .map(response_from_row)
                .collect::<Result<Vec<_>>>();
        }
        anyhow::ensure!(
            actual.is_subset(&expected),
            "terminal results contained workflows outside the selected inventory: {:?}",
            actual.difference(&expected).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    dump_workflow_timeout_diagnostics(pool, "aggregate-terminal-results").await;
    anyhow::bail!(
        "timed out waiting for {} completed workflows",
        expected_workflows.len()
    )
}

pub(super) async fn load_terminal_result(
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

pub(super) type TerminalResultRow = (
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

pub(super) fn response_from_row(
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
