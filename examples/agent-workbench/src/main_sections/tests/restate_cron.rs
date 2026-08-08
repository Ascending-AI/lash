fn rotate_cron_session_out_of_current(state: &AppState) -> String {
    let cron_session_id = state.current_session_id();
    let (rotated_session_id, new_current_session_id) = state.session_ids.rotate();
    assert_eq!(rotated_session_id, cron_session_id);
    assert_ne!(new_current_session_id, cron_session_id);
    cron_session_id
}

fn cron_job_key_for_session(state: &AppState, session_id: &str) -> String {
    let prefix = format!("{session_id}:");
    let mut matching = state
        .restate_cron_job_keys
        .lock()
        .expect("cron job key lock")
        .iter()
        .filter(|job_key| job_key.starts_with(&prefix))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one cron job key for rotated session `{session_id}`, got {matching:?}"
    );
    matching.pop().expect("one matching cron job key")
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
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count}+ `{name}` records for session `{session_id}` and job `{job_key}` within {timeout:?}, saw {seen}"
        );
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
    wait_for_cron_trace_record_count(
        trace_path,
        "agent_workbench.cron.restate.zombie_cancelled",
        cron_session_id,
        job_key,
        1,
        Duration::from_secs(30),
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

async fn assert_two_live_ticks_then_retire(
    state: &AppState,
    trace_path: &std::path::Path,
    cron_session_id: &str,
    job_key: &str,
) {
    wait_for_cron_trace_record_count(
        trace_path,
        "agent_workbench.cron.restate.run",
        cron_session_id,
        job_key,
        2,
        Duration::from_secs(30),
    )
    .await;
    assert_live_non_current_cron_trace(trace_path, cron_session_id, job_key);
    retire_cron_session_and_assert_zombie(state, trace_path, cron_session_id, job_key).await;
}
