const LIVE_RESTATE_CRON_SCHEDULE_INTERVAL: Duration = Duration::from_secs(2);
const LIVE_RESTATE_CRON_JITTER_MARGIN: Duration = Duration::from_secs(60);
const LIVE_RESTATE_CRON_ZOMBIE_EXPR: &str = "0 0 0 1 1 *";

fn live_restate_cron_tick_wait() -> Duration {
    LIVE_RESTATE_CRON_SCHEDULE_INTERVAL
        .saturating_mul(2)
        .saturating_add(LIVE_RESTATE_CRON_JITTER_MARGIN)
}

fn test_cron_trigger_source(expr: &str) -> String {
    format!(
        r#"
        process remember_tick(tick: cron.Tick) {{
          wake {{ kind: "cron_tick", fired_at: tick.fired_at }}
          finish {{ fired_at: tick.fired_at }}
        }}

        handle = await triggers.register({{
          source: cron.Schedule({{ expr: "{expr}", tz: "UTC" }}),
          target: remember_tick,
          inputs: {{ tick: trigger.event }},
          name: "cron smoke"
        }})?
        finish "cron registered"
        "#
    )
}

fn live_restate_cron_provider(expr: String) -> ProviderHandle {
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_provider = Arc::clone(&response_index);
    lash::testing::TestProvider::builder()
        .kind("workbench-restate-cron-e2e")
        .complete(move |_| {
            let response_index = Arc::clone(&response_index_for_provider);
            let expr = expr.clone();
            async move {
                match response_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => Ok(text_response(&format!(
                        "<lashlang>\n{}\n</lashlang>",
                        test_cron_trigger_source(&expr).trim()
                    ))),
                    other => panic!(
                        "future-scheduled zombie-path provider must not receive cron turn call {other}"
                    ),
                }
            }
        })
        .build()
        .into_handle()
}

fn gated_live_restate_cron_provider(
) -> (
    ProviderHandle,
    mpsc::UnboundedReceiver<usize>,
    Arc<tokio::sync::Notify>,
) {
    let (entered_tx, entered_rx) = mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let release_for_provider = Arc::clone(&release);
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_provider = Arc::clone(&response_index);
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-restate-cron-sync-cancel-e2e")
        .complete(move |_| {
            let entered_tx = entered_tx.clone();
            let release = Arc::clone(&release_for_provider);
            let response_index = Arc::clone(&response_index_for_provider);
            async move {
                let call = response_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 1 {
                    let _ = entered_tx.send(call);
                    release.notified().await;
                }
                Ok(if call == 0 {
                    text_response(&format!(
                        "<lashlang>\n{}\n</lashlang>",
                        test_cron_trigger_source(&format!(
                            "*/{} * * * * *",
                            LIVE_RESTATE_CRON_SCHEDULE_INTERVAL.as_secs()
                        ))
                        .trim()
                    ))
                } else {
                    text_response("<lashlang>\nfinish \"cron tick observed\"\n</lashlang>")
                })
            }
        })
        .build()
        .into_handle();
    (provider, entered_rx, release)
}

struct LiveRestateCronScenario {
    data_dir: PathBuf,
    state: AppState,
    trace_path: PathBuf,
    cron_session_id: String,
    cron_job_key: String,
}

async fn start_live_restate_cron_scenario(
    data_dir_label: &str,
    provider: ProviderHandle,
) -> LiveRestateCronScenario {
    // The cron scenarios are `#[ignore]`d, so they only run when the recipe
    // asked for them. Skipping on an absent ingress URL would report the live
    // E2E green having exercised nothing; fail instead, as the recovery
    // scenarios in the sibling file do.
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
    let admin_url = std::env::var("RESTATE_ADMIN_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
    let endpoint_bind: SocketAddr = std::env::var("AGENT_WORKBENCH_E2E_ENDPOINT_BIND")
        .unwrap_or_else(|_| "127.0.0.1:19081".to_string())
        .parse()
        .expect("valid workbench E2E endpoint bind");
    let endpoint_url = std::env::var("AGENT_WORKBENCH_E2E_ENDPOINT_URL")
        .unwrap_or_else(|_| format!("http://{endpoint_bind}"));
    let data_dir = std::env::temp_dir().join(format!(
        "{data_dir_label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let LiveWorkbenchRestateHarness {
        state,
        process_worker,
        process_deployment,
        trace_path,
    } = live_workbench_restate_state_with_provider(
        &data_dir,
        ingress_url,
        provider,
        WorkbenchSessions::fresh(),
        ActiveTurns::default(),
    )
    .await;
    restate::spawn_restate_endpoint(
        endpoint_bind,
        state.clone(),
        process_deployment,
        process_worker,
    );
    wait_for_endpoint_socket(endpoint_bind).await;
    register_restate_deployment(&admin_url, &endpoint_url).await;
    let turn_invocation_id = run_workbench_turn_via_restate(
        &state,
        "Register the cron trigger used by this cancellation-path test.",
    )
    .await;
    wait_for_workbench_message(&state, "cron registered", Duration::from_secs(60)).await;
    wait_for_restate_invocation_success(
        &state,
        &turn_invocation_id,
        Duration::from_secs(30),
    )
    .await;
    wait_for_restate_cron_sync(&state, &trace_path, Duration::from_secs(30)).await;
    let cron_session_id = rotate_cron_session_out_of_current(&state);
    let cron_job_key = cron_job_key_for_session(&state, &cron_session_id);
    LiveRestateCronScenario {
        data_dir,
        state,
        trace_path,
        cron_session_id,
        cron_job_key,
    }
}

async fn disable_cron_registration_for_sync_scenario(scenario: &LiveRestateCronScenario) {
    let records = scenario
        .state
        .trigger_store
        .list_subscriptions(lash::triggers::TriggerSubscriptionFilter::for_session(
            &scenario.cron_session_id,
        ))
        .await
        .expect("list cron registration before queued-turn sync cancel");
    let [record] = records.as_slice() else {
        panic!("expected one cron registration before sync cancel, got {records:#?}");
    };
    scenario
        .state
        .trigger_store
        .execute_command(
            &format!("fig1130-sync-disable-{}", uuid::Uuid::new_v4()),
            lash::triggers::TriggerCommand::Disable {
                owner_scope: record.owner_scope.clone(),
                actor: record.registrant.clone(),
                subscription_key: record.subscription_key.clone(),
                expected_revision: record.revision,
            },
        )
        .await
        .expect("execute cron registration disable effect")
        .expect("disable cron registration before queued-turn sync");
}

async fn assert_queued_turn_sync_cancelled(scenario: &LiveRestateCronScenario) {
    wait_for_cron_trace_record_count(
        &scenario.trace_path,
        "agent_workbench.cron.restate.sync_cancelled",
        &scenario.cron_session_id,
        &scenario.cron_job_key,
        1,
        live_restate_cron_tick_wait(),
    )
    .await;
    let sync_records = cron_trace_records_for_job(
        &scenario.trace_path,
        "agent_workbench.cron.restate.sync_cancelled",
        &scenario.cron_session_id,
        &scenario.cron_job_key,
    );
    let sync_record = sync_records.last().expect("queued-turn sync cancel trace");
    assert_eq!(
        sync_record.pointer("/payload/reason").and_then(Value::as_str),
        Some("queued_turn")
    );
    assert_restate_cron_job_cancelled(&scenario.state, &scenario.cron_job_key).await;
}

fn rotate_cron_session_out_of_current(state: &AppState) -> String {
    let cron_session_id = state.current_session_id();
    let (rotated_session_id, new_current_session_id) = state.sessions.rotate();
    assert_eq!(rotated_session_id, cron_session_id);
    assert_ne!(new_current_session_id, cron_session_id);
    cron_session_id
}

fn cron_job_key_for_session(state: &AppState, session_id: &str) -> String {
    let guard = state
        .restate_cron_job_keys
        .lock_recover();
    let matching = guard
        .get(session_id)
        .unwrap_or_else(|| panic!("missing cron job keys for session `{session_id}`"));
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one cron job key for rotated session `{session_id}`, got {matching:?}"
    );
    matching.iter().next().expect("one matching cron job key").clone()
}

fn cron_trace_records_for_job(
    trace_path: &std::path::Path,
    name: &str,
    session_id: &str,
    job_key: &str,
) -> Vec<Value> {
    std::fs::read_to_string(trace_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record.get("name").and_then(Value::as_str) == Some(name)
                && record
                    .pointer("/payload/job_session_id")
                    .and_then(Value::as_str)
                    == Some(session_id)
                && record
                    .pointer("/payload/job_key")
                    .and_then(Value::as_str)
                    == Some(job_key)
        })
        .collect()
}

fn cron_trace_timeline_for_job(
    trace_path: &std::path::Path,
    session_id: &str,
    job_key: &str,
) -> String {
    let timeline = std::fs::read_to_string(trace_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.starts_with("agent_workbench.cron.restate."))
                && record
                    .pointer("/context/session_id")
                    .and_then(Value::as_str)
                    == Some(session_id)
                && record.pointer("/payload/job_key").and_then(Value::as_str) == Some(job_key)
        })
        .map(|record| {
            let timestamp = record
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or("<missing trace timestamp>");
            let name = record
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<missing trace name>");
            let payload = record.get("payload").cloned().unwrap_or(Value::Null);
            format!("{timestamp} {name} payload={payload}")
        })
        .collect::<Vec<_>>();
    if timeline.is_empty() {
        format!(
            "<no cron records for session `{session_id}` and job `{job_key}` at {}>",
            trace_path.display()
        )
    } else {
        timeline.join("\n")
    }
}

async fn wait_for_cron_workbench_message(
    state: &AppState,
    trace_path: &std::path::Path,
    session_id: &str,
    job_key: &str,
    needle: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let messages = state.messages_snapshot();
        if messages
            .iter()
            .any(|message| message.role == "assistant" && message.text.contains(needle))
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "expected workbench message containing `{needle}` within {timeout:?}; messages={messages:#?}; observed cron tick timeline:\n{}",
                cron_trace_timeline_for_job(trace_path, session_id, job_key)
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_cron_trace_record_count(
    trace_path: &std::path::Path,
    name: &str,
    session_id: &str,
    job_key: &str,
    count: usize,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let seen = cron_trace_records_for_job(trace_path, name, session_id, job_key).len();
        if seen >= count {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "expected {count}+ `{name}` records for session `{session_id}` and job `{job_key}` within {timeout:?}, saw {seen}; observed cron tick timeline:\n{}",
                cron_trace_timeline_for_job(trace_path, session_id, job_key)
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn assert_live_non_current_cron_trace(
    trace_path: &std::path::Path,
    session_id: &str,
    job_key: &str,
) {
    let records = cron_trace_records_for_job(
        trace_path,
        "agent_workbench.cron.restate.run",
        session_id,
        job_key,
    );
    let record = records.last().expect("live cron trace for rotated session");
    assert_eq!(
        record
            .pointer("/payload/decision_basis")
            .and_then(Value::as_str),
        Some("session_store_meta_present")
    );
    assert_eq!(
        record
            .pointer("/payload/session_state")
            .and_then(Value::as_str),
        Some("live")
    );
}

async fn retire_cron_session_and_assert_zombie(
    state: &AppState,
    trace_path: &std::path::Path,
    cron_session_id: &str,
    job_key: &str,
) {
    let execution_scope = state
        .core
        .session_delete_scope(cron_session_id)
        .await
        .expect("resolve cron session delete scope");
    let delete_invocation_id = restate::submit_session_delete(
        state,
        restate::WorkbenchSessionDeleteWorkflowRequest {
            operation_id: format!("workbench-delete-{}", uuid::Uuid::new_v4()),
            session_id: cron_session_id.to_string(),
            execution_scope,
        },
    )
    .await
    .expect("submit cron session retirement");
    wait_for_restate_invocation_success(
        state,
        &delete_invocation_id,
        Duration::from_secs(20),
    )
    .await;
    lash_restate::RestateIngressClient::new(lash_restate::RestateConnection::with_client(
        &state.restate_ingress_url,
        state.restate_http.clone(),
    ))
    .call_object_empty("WorkbenchCronJob", job_key, "run")
    .await
    .expect("drive retired cron job run");
    wait_for_cron_trace_record_count(
        trace_path,
        "agent_workbench.cron.restate.zombie_cancelled",
        cron_session_id,
        job_key,
        1,
        live_restate_cron_tick_wait(),
    )
    .await;
    let info_url = format!(
        "{}/WorkbenchCronJob/{job_key}/info",
        state.restate_ingress_url.trim_end_matches('/')
    );
    let info = state
        .restate_http
        .post(info_url)
        .send()
        .await
        .expect("query retired cron job info")
        .error_for_status()
        .expect("retired cron job info status")
        .json::<serde_json::Value>()
        .await
        .expect("decode retired cron job info");
    assert!(
        info.is_null(),
        "retired cron tick must clear Restate cron state, got {info}"
    );
    let records = cron_trace_records_for_job(
        trace_path,
        "agent_workbench.cron.restate.zombie_cancelled",
        cron_session_id,
        job_key,
    );
    let record = records.last().expect("retired cron trace for rotated session");
    assert_eq!(
        record
            .pointer("/payload/session_state")
            .and_then(Value::as_str),
        Some("retired")
    );
    assert_eq!(
        record.pointer("/payload/reason").and_then(Value::as_str),
        Some("session_retired")
    );
}

async fn assert_restate_cron_job_cancelled(
    state: &AppState,
    job_key: &str,
) {
    let info_url = format!(
        "{}/WorkbenchCronJob/{job_key}/info",
        state.restate_ingress_url.trim_end_matches('/')
    );
    let info = state
        .restate_http
        .post(info_url)
        .send()
        .await
        .expect("query cancelled cron job info")
        .error_for_status()
        .expect("cancelled cron job info status")
        .json::<serde_json::Value>()
        .await
        .expect("decode cancelled cron job info");
    assert!(
        info.is_null(),
        "cancelled cron job must clear Restate cron state, got {info}"
    );
}
