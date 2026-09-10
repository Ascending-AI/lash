use super::*;

pub(super) fn assert_kitchen_sink_response(
    response: &TurnResponse,
    expect_attachment: bool,
) -> Result<()> {
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

pub(super) fn assert_queued_wake_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_trigger_setup_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_signal_suspend_setup_response(response: &TurnResponse) -> Result<ProcessId> {
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
    Ok(ProcessId::from(process_id))
}

pub(super) fn assert_async_completion_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_durable_input_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_process_llm_query_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_parent_durable_input_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_tool_batch_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_segment_loop_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_frame_switch_queued_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_frame_switch_crash_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_frame_switch_prepared_response(response: &TurnResponse) -> Result<()> {
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

pub(super) fn assert_frame_switch_cancel_response(response: &TurnResponse) -> Result<()> {
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

pub(super) async fn assert_frame_switch_provider_order(pool: &sqlx::PgPool) -> Result<()> {
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

pub(super) async fn wait_for_durable_input_key(
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

pub(super) async fn wait_for_durable_wait_suspended(
    admin_url: &str,
    key: &AwaitEventKey,
) -> Result<()> {
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

pub(super) async fn wait_for_durable_wait_attached(
    admin_url: &str,
    key: &AwaitEventKey,
) -> Result<()> {
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

pub(super) async fn engine_conformance_key(
    host: &RestateEffectHost,
    ordering: &str,
) -> Result<AwaitEventKey> {
    host.await_event_key(
        &ExecutionScope::runtime_operation(format!("workers-e2e-promise-conformance-{ordering}")),
        AwaitEventWaitIdentity::Custom {
            key: ordering.to_string(),
        },
    )
    .await
    .with_context(|| format!("build `{ordering}` engine conformance key"))
}

pub(super) async fn await_durable_wait_on_worker(
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
