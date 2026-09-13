use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use lash_postgres_store::PostgresStorage;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_restate_postgres_workers_e2e::{
    EXPECTED_ASYNC_TEXT, EXPECTED_DURABLE_INPUT_TEXT, EXPECTED_FINAL_TEXT,
    EXPECTED_FRAME_SWITCH_CANCEL_TEXT, EXPECTED_FRAME_SWITCH_TEXT,
    EXPECTED_PARENT_DURABLE_INPUT_TEXT, EXPECTED_SEGMENT_LOOP_TEXT, EXPECTED_TOOL_BATCH_TEXT,
    EXPECTED_WAKE_TEXT, ensure_e2e_schema, env, record_provider_call, required_env,
};

#[derive(Clone)]
struct AppState {
    calls: Arc<AtomicU64>,
    pool: PgPool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let database_url = required_env("DATABASE_URL")?;
    let storage = PostgresStorage::connect(&database_url).await?;
    ensure_e2e_schema(storage.pool()).await?;
    let port = env("MOCK_PROVIDER_PORT", "18001");
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/chat/completions", post(chat_completion))
        .route("/v1/responses", post(responses))
        .with_state(AppState {
            calls: Arc::new(AtomicU64::new(0)),
            pool: storage.pool().clone(),
        });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn chat_completion(State(state): State<AppState>, Json(request): Json<Value>) -> Json<Value> {
    let n = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    let request_id = format!("chatcmpl-e2e-{n}");
    let latest_user = latest_user_text(&request);
    let full_text = request.to_string();
    let workflow_id = extract_latest_marker(&latest_user, "workflow_id=")
        .or_else(|| extract_latest_marker(&full_text, "workflow_id="))
        .unwrap_or_else(|| "unknown".to_string());
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let scenario = latest_scenario_marker(&latest_user)
        .or_else(|| latest_scenario_marker(&full_text))
        .unwrap_or(MockScenario::KitchenSink);
    let fail_once = latest_fail_once_marker(&latest_user)
        .or_else(|| latest_fail_once_marker(&full_text))
        .unwrap_or(false);
    let is_llm_query_direct = full_text.contains("llm_query_result");
    let (scenario, content) = if is_llm_query_direct {
        (
            "process_llm_query_direct",
            r#"{"kind":"value","value":{"category":"personal","confidence":0.98},"error":null}"#
                .to_string(),
        )
    } else {
        match scenario {
            MockScenario::TriggerSetup => ("trigger_setup", trigger_setup_script()),
            MockScenario::QueuedWake => ("queued_wake", queued_wake_script()),
            MockScenario::SignalSuspend => ("signal_suspend", signal_suspend_script(&workflow_id)),
            MockScenario::AsyncCompletion => {
                ("async_completion", async_completion_script(&workflow_id))
            }
            MockScenario::ProcessLlmQuery => (
                "process_llm_query",
                process_llm_query_script(&workflow_id, fail_once),
            ),
            MockScenario::DurableInputRequest => (
                "durable_input_request",
                durable_input_request_script(&workflow_id),
            ),
            MockScenario::ParentDurableInputAfterChild => (
                "parent_durable_input_after_child",
                parent_durable_input_after_child_script(&workflow_id),
            ),
            MockScenario::ToolBatch => ("tool_batch", tool_batch_script(&workflow_id, fail_once)),
            MockScenario::SegmentLoop => ("segment_loop", segment_loop_script(&workflow_id)),
            MockScenario::FrameSwitchQueuedStart => (
                "frame_switch_queued_start",
                frame_switch_start_script(&workflow_id, "frame_switch_queued_follow=true"),
            ),
            MockScenario::FrameSwitchPreparedStart => (
                "frame_switch_prepared_start",
                frame_switch_start_script(&workflow_id, "frame_switch_prepared_follow=true"),
            ),
            MockScenario::FrameSwitchCrashStart => (
                "frame_switch_crash_start",
                frame_switch_start_script(&workflow_id, "frame_switch_crash_follow=true"),
            ),
            MockScenario::FrameSwitchCancelStart => (
                "frame_switch_cancel_start",
                frame_switch_start_script(&workflow_id, "frame_switch_cancel_follow=true"),
            ),
            MockScenario::FrameSwitchQueuedFollow => (
                "frame_switch_queued_follow",
                frame_switch_follow_script(&workflow_id),
            ),
            MockScenario::FrameSwitchPreparedFollow => (
                "frame_switch_prepared_follow",
                frame_switch_follow_script(&workflow_id),
            ),
            MockScenario::FrameSwitchCrashFollow => (
                "frame_switch_crash_follow",
                frame_switch_follow_script(&workflow_id),
            ),
            MockScenario::FrameSwitchCancelFollow => (
                "frame_switch_cancel_follow",
                frame_switch_cancel_follow_script(&workflow_id),
            ),
            MockScenario::FrameSwitchPending => (
                "frame_switch_pending",
                frame_switch_pending_script(&workflow_id),
            ),
            MockScenario::FrameSwitchPostCancel => (
                "frame_switch_post_cancel",
                frame_switch_post_cancel_script(&workflow_id),
            ),
            MockScenario::TurnControlHold => (
                "turn_control_hold",
                turn_control_hold_script(&workflow_id, fail_once),
            ),
            MockScenario::TurnControlSleep => (
                "turn_control_sleep",
                turn_control_sleep_script(&workflow_id),
            ),
            MockScenario::TurnControlComplete => (
                "turn_control_complete",
                turn_control_complete_script(&workflow_id),
            ),
            MockScenario::KitchenSink => {
                ("kitchen_sink", kitchen_sink_script(&workflow_id, fail_once))
            }
        }
    };
    let response = json!({
        "id": request_id,
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": content,
            }
        }],
        "usage": {
            "prompt_tokens": 17,
            "completion_tokens": 31,
            "total_tokens": 48
        }
    });
    if let Err(err) = record_provider_call(
        &state.pool,
        response["id"].as_str().unwrap_or("chatcmpl-e2e"),
        scenario,
        &workflow_id,
        model,
        &request,
        &response,
    )
    .await
    {
        tracing::error!(workflow_id, scenario, error = %err, "failed to record provider call");
    }
    if scenario == "frame_switch_queued_start" {
        // Hold the first physical turn after the provider boundary is observable
        // so the worker can enqueue a second item while the logical chain is
        // active, before the switch commit claims anything else.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Json(response)
}

async fn responses(State(state): State<AppState>, Json(request): Json<Value>) -> Json<Value> {
    let n = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    let request_id = format!("resp-e2e-{n}");
    let response = json!({
        "id": request_id,
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": request.get("model").and_then(Value::as_str).unwrap_or("e2e-mock"),
        "output": [{
            "id": format!("msg-e2e-{n}"),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": "ok"
            }]
        }],
        "usage": {
            "input_tokens": 1,
            "output_tokens": 1,
            "total_tokens": 2
        }
    });
    Json(response)
}

fn latest_user_text(request: &Value) -> String {
    request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        })
        .map(message_content_text)
        .unwrap_or_else(|| request.to_string())
}

fn message_content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.map(Value::to_string).unwrap_or_default(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MockScenario {
    KitchenSink,
    QueuedWake,
    TriggerSetup,
    SignalSuspend,
    AsyncCompletion,
    ProcessLlmQuery,
    DurableInputRequest,
    ParentDurableInputAfterChild,
    ToolBatch,
    SegmentLoop,
    FrameSwitchQueuedStart,
    FrameSwitchPreparedStart,
    FrameSwitchCrashStart,
    FrameSwitchCancelStart,
    FrameSwitchQueuedFollow,
    FrameSwitchPreparedFollow,
    FrameSwitchCrashFollow,
    FrameSwitchCancelFollow,
    FrameSwitchPending,
    FrameSwitchPostCancel,
    TurnControlHold,
    TurnControlSleep,
    TurnControlComplete,
}

fn latest_scenario_marker(text: &str) -> Option<MockScenario> {
    [
        (
            "frame_switch_prepared_follow=true",
            MockScenario::FrameSwitchPreparedFollow,
        ),
        (
            "frame_switch_cancel_follow=true",
            MockScenario::FrameSwitchCancelFollow,
        ),
        (
            "frame_switch_crash_follow=true",
            MockScenario::FrameSwitchCrashFollow,
        ),
        (
            "frame_switch_queued_follow=true",
            MockScenario::FrameSwitchQueuedFollow,
        ),
        (
            "frame_switch_prepared_start=true",
            MockScenario::FrameSwitchPreparedStart,
        ),
        (
            "frame_switch_queued_start=true",
            MockScenario::FrameSwitchQueuedStart,
        ),
        (
            "frame_switch_crash_start=true",
            MockScenario::FrameSwitchCrashStart,
        ),
        (
            "frame_switch_cancel_start=true",
            MockScenario::FrameSwitchCancelStart,
        ),
        (
            "frame_switch_pending=true",
            MockScenario::FrameSwitchPending,
        ),
        (
            "frame_switch_post_cancel=true",
            MockScenario::FrameSwitchPostCancel,
        ),
        ("turn_control_hold=true", MockScenario::TurnControlHold),
        ("turn_control_sleep=true", MockScenario::TurnControlSleep),
        (
            "turn_control_complete=true",
            MockScenario::TurnControlComplete,
        ),
        ("segment_loop=true", MockScenario::SegmentLoop),
        ("tool_batch=true", MockScenario::ToolBatch),
        (
            "durable_input_request=true",
            MockScenario::DurableInputRequest,
        ),
        (
            "parent_durable_input_after_child=true",
            MockScenario::ParentDurableInputAfterChild,
        ),
        ("async_completion=true", MockScenario::AsyncCompletion),
        ("process_llm_query=true", MockScenario::ProcessLlmQuery),
        ("trigger_setup=true", MockScenario::TriggerSetup),
        ("signal_suspend=true", MockScenario::SignalSuspend),
        ("Background process wake", MockScenario::QueuedWake),
        ("kitchen_sink=true", MockScenario::KitchenSink),
    ]
    .into_iter()
    .filter_map(|(marker, scenario)| text.rfind(marker).map(|idx| (idx, scenario)))
    .max_by_key(|(idx, _)| *idx)
    .map(|(_, scenario)| scenario)
}

fn turn_control_hold_script(workflow_id: &str, fail_once: bool) -> String {
    let crash = if fail_once {
        format!(
            r#"
crash = await tools.crash_once({{ workflow_id: "{workflow_id}" }})?
"#
        )
    } else {
        String::new()
    };
    format!(
        r#"
Wait for the exact-turn cooperative cancellation gate.

<lashlang>
{crash}gate = await tools.cancel_gate({{ workflow_id: "{workflow_id}" }})?
finish {{ gate: gate, final: "unreachable" }}
</lashlang>
"#
    )
}

fn turn_control_sleep_script(workflow_id: &str) -> String {
    format!(
        r#"
Enter a long durable timer that must be woken by exact-turn cancellation.

<lashlang>
sleep for "300s"
finish {{ workflow_id: "{workflow_id}", final: "unreachable" }}
</lashlang>
"#
    )
}

fn turn_control_complete_script(workflow_id: &str) -> String {
    format!(
        r#"
Complete immediately so the terminal seal can race cancellation.

<lashlang>
finish {{ workflow_id: "{workflow_id}", final: "turn-control-completed" }}
</lashlang>
"#
    )
}

fn frame_switch_start_script(workflow_id: &str, follow_marker: &str) -> String {
    format!(
        r#"
Switch to a fresh frame and carry the non-empty baton seed.

<lashlang>
await control.continue_as({{
  task: "Complete the durable follow-on. workflow_id={workflow_id} {follow_marker}",
  seed: {{ baton: "seed:{workflow_id}" }}
}})?
</lashlang>
"#
    )
}

fn frame_switch_follow_script(workflow_id: &str) -> String {
    format!(
        r#"
Read the seeded baton and finish.

<lashlang>
finish {{
  workflow_id: "{workflow_id}",
  seed_visible: baton,
  follow_on: true,
  final: "{EXPECTED_FRAME_SWITCH_TEXT}"
}}
</lashlang>
"#
    )
}

fn frame_switch_cancel_follow_script(workflow_id: &str) -> String {
    format!(
        r#"
Wait for cancellation in the follow-on frame.

<lashlang>
gate = await tools.cancel_gate({{ workflow_id: "{workflow_id}" }})?
finish {{ gate: gate, final: "unreachable" }}
</lashlang>
"#
    )
}

fn frame_switch_pending_script(workflow_id: &str) -> String {
    format!(
        r#"
Finish the pre-existing second queued item.

<lashlang>
finish {{
  workflow_id: "{workflow_id}",
  pending_item: true,
  final: "pending-after-frame-switch"
}}
</lashlang>
"#
    )
}

fn frame_switch_post_cancel_script(workflow_id: &str) -> String {
    format!(
        r#"
Prove the cancelled session remains usable.

<lashlang>
finish {{
  workflow_id: "{workflow_id}",
  session_usable: true,
  final: "{EXPECTED_FRAME_SWITCH_CANCEL_TEXT}"
}}
</lashlang>
"#
    )
}

fn extract_latest_marker(text: &str, marker: &str) -> Option<String> {
    let start = text.rfind(marker)? + marker.len();
    let rest = &text[start..];
    let value: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
        .collect();
    (!value.is_empty()).then_some(value)
}

fn latest_fail_once_marker(text: &str) -> Option<bool> {
    let value = extract_latest_marker(text, "fail_once=")?;
    match value.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn segment_loop_script(workflow_id: &str) -> String {
    format!(
        r#"
Execute the same authored loop once without segmentation and once with a forced
three-effect segment budget.

<lashlang>
process effect_loop(tools: Tools, workflow_id: str, force_segmentation: bool) {{
  n = 0
  total = 0
  values = []
  while n < 8 {{
    lookup = await tools.app_lookup({{ key: "segment-loop" }})?
    total = total + n
    values = values + [lookup.value]
    n = n + 1
  }}
  finish {{ total: total, values: values }}
}}

control_handle = start effect_loop(
  tools: tools,
  workflow_id: "{workflow_id}",
  force_segmentation: false
)
segmented_handle = start effect_loop(
  tools: tools,
  workflow_id: "{workflow_id}",
  force_segmentation: true
)
control = (await control_handle)?
segmented = (await segmented_handle)?
finish {{
  workflow_id: "{workflow_id}",
  control: control,
  segmented: segmented,
  final: "{EXPECTED_SEGMENT_LOOP_TEXT}"
}}
</lashlang>
"#
    )
}

fn kitchen_sink_script(workflow_id: &str, fail_once: bool) -> String {
    let crash = if fail_once {
        format!(
            r#"
crash = await tools.crash_once({{ workflow_id: "{workflow_id}" }})?
"#
        )
    } else {
        String::new()
    };
    format!(
        r#"
Execute this program.

<lashlang>
process child(tools: Tools, value: str) {{
  lookup = await tools.app_lookup({{ key: value }})?
  finish {{ child: value, lookup: lookup.value }}
}}

process parent(tools: Tools, workflow_id: str) {{
  parent_lookup = await tools.app_lookup({{ key: "parent" }})?
  nested_handle = start child(tools: tools, value: "nested")
  nested_result = (await nested_handle)?
  parallel = await {{
    left: start child(tools: tools, value: "left"),
    right: start child(tools: tools, value: "right")
  }}
  left_result = parallel.left?
  right_result = parallel.right?
  sleep for "1ms"
  finish {{
    parent_lookup: parent_lookup.value,
    nested: nested_result.lookup,
    parallel: {{
      left: left_result.lookup,
      right: right_result.lookup
    }},
    slept: true,
    wake: "deferred"
  }}
}}

process waker(workflow_id: str) {{
  sleep for "1500ms"
  wake {{
    kind: "parent_wake",
    workflow_id: workflow_id,
    text: "deploy complete"
  }}
  finish {{ wake: "sent" }}
}}

foreground = await tools.app_lookup({{ key: "foreground" }})?
attachment = await tools.make_attachment({{
  workflow_id: "{workflow_id}",
  name: "kitchen-sink.png"
}})?
{crash}
parent_handle = start parent(tools: tools, workflow_id: "{workflow_id}")
process_result = (await parent_handle)?
waker_handle = start waker(workflow_id: "{workflow_id}")
sleep for "0ms"
finish {{
  workflow_id: "{workflow_id}",
  foreground: foreground.value,
  attachment_id: attachment.id,
  attachment_mime: attachment.mime,
  wake_process: waker_handle,
  process: process_result,
  final: "{EXPECTED_FINAL_TEXT}"
}}
</lashlang>
"#
    )
}

fn trigger_setup_script() -> String {
    r#"
Register this trigger.

<lashlang>
process on_button(event: ui.button.Pressed) {
  finish { triggered: event.button, message: event.message }
}

handle = await triggers.register({
  source: ui.button.pressed({}),
  target: on_button,
  inputs: { event: trigger.event },
  name: "button watcher"
})?
finish { registered: true, handle: handle }
</lashlang>
"#
    .to_string()
}

fn signal_suspend_script(workflow_id: &str) -> String {
    format!(
        r#"
Start the signal suspension process but do not await it.

<lashlang>
process waiter(workflow_id: str) signals {{ first: any, second: any }} {{
  first = wait_signal("first")
  second = wait_signal("second")
  finish {{
    workflow_id: workflow_id,
    first: first,
    second: second
  }}
}}

handle = start waiter(workflow_id: "{workflow_id}")
finish {{
  workflow_id: "{workflow_id}",
  process_id: handle.id,
  final: "signal-suspend-started"
}}
</lashlang>
"#
    )
}

fn queued_wake_script() -> String {
    format!(
        r#"
Consume the queued wake.

<lashlang>
finish {{ wake_consumed: true, final: "{EXPECTED_WAKE_TEXT}" }}
</lashlang>
"#
    )
}

fn async_completion_script(workflow_id: &str) -> String {
    format!(
        r#"
Exercise the async host tool completion path.

<lashlang>
process async_child(tools: Tools, workflow_id: str) {{
  lookup = await tools.async_lookup({{ workflow_id: workflow_id, key: "detached" }})?
  finish lookup
}}

handle = start async_child(tools: tools, workflow_id: "{workflow_id}")
result = (await handle)?
finish {{
  workflow_id: "{workflow_id}",
  async: result,
  final: "{EXPECTED_ASYNC_TEXT}"
}}
</lashlang>
"#
    )
}

fn process_llm_query_script(workflow_id: &str, fail_once: bool) -> String {
    let replay_probe = if fail_once {
        // `peer_takeover: false`: this scenario proves journal replay on the
        // reincarnated worker, so the crashed endpoint must come straight back
        // rather than waiting on a peer that cedes (FIG-3030).
        format!(
            r#"
  await tools.crash_once({{ workflow_id: "{workflow_id}", peer_takeover: false }})?"#
        )
    } else {
        String::new()
    };
    format!(
        r#"
Exercise the exact FIG-446 process-to-llm_query geometry with typed output.

<lashlang>
process enrich(event: {{ email: str }}) {{
  enriched = await llm.query({{
    task: "Classify this email. workflow_id={workflow_id} process_llm_query=true",
    inputs: {{ event: event }},
    output: Type {{ category: str, confidence: float }}
  }})?{replay_probe}
  finish enriched
}}
handle = start enrich(event: {{ email: "hello@example.com" }})
result = (await handle)?
finish {{
  workflow_id: "{workflow_id}",
  category: result.category,
  confidence: result.confidence,
  final: "process-llm-query-complete"
}}
</lashlang>
"#
    )
}

fn durable_input_request_script(workflow_id: &str) -> String {
    format!(
        r#"
Exercise a durable in-process tool that opens an input request and resumes through a custom await key.

<lashlang>
process durable_child(tools: Tools, workflow_id: str) {{
  input = await tools.durable_input_request({{
    workflow_id: workflow_id,
    question: "approve durable input?"
  }})?
  finish input
}}

handle = start durable_child(tools: tools, workflow_id: "{workflow_id}")
result = (await handle)?
finish {{
  workflow_id: "{workflow_id}",
  durable: result,
  final: "{EXPECTED_DURABLE_INPUT_TEXT}"
}}
</lashlang>
"#
    )
}

fn parent_durable_input_after_child_script(workflow_id: &str) -> String {
    format!(
        r#"
Exercise parent replay after a completed child process and a durable input suspension.

<lashlang>
process immediate_child(value: str) {{
  finish {{ child: value }}
}}

process parent(tools: Tools, workflow_id: str) {{
  child_handle = start immediate_child(value: "ready")
  child = (await child_handle)?
  input = await tools.durable_input_request({{
    workflow_id: workflow_id,
    question: "approve parent durable input?",
    attach_after_resolution: true
  }})?
  finish {{
    child: child.child,
    durable: input
  }}
}}

handle = start parent(tools: tools, workflow_id: "{workflow_id}")
result = (await handle)?
finish {{
  workflow_id: "{workflow_id}",
  parent: result,
  final: "{EXPECTED_PARENT_DURABLE_INPUT_TEXT}"
}}
</lashlang>
"#
    )
}

fn tool_batch_script(workflow_id: &str, fail_once: bool) -> String {
    let crash = if fail_once {
        format!(
            r#"
crash = await tools.crash_once({{ workflow_id: "{workflow_id}" }})?
"#
        )
    } else {
        String::new()
    };
    format!(
        r#"
Exercise direct aggregate resource batching.

<lashlang>
{crash}
batch = await {{
  slow: tools.batch_side_effect({{
    workflow_id: "{workflow_id}",
    key: "slow",
    delay_ms: 75
  }})?,
  fast: tools.batch_side_effect({{
    workflow_id: "{workflow_id}",
    key: "fast",
    delay_ms: 5
  }})?,
  literal: "kept"
}}
finish {{
  workflow_id: "{workflow_id}",
  batch: batch,
  final: "{EXPECTED_TOOL_BATCH_TEXT}"
}}
</lashlang>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_fail_once_marker_uses_last_prompt_marker() {
        let text = "workflow_id=e2e-failover fail_once=true\n\
            workflow_id=e2e-tool-batch tool_batch=true fail_once=false";

        assert_eq!(latest_fail_once_marker(text), Some(false));
    }

    #[test]
    fn latest_fail_once_marker_accepts_true_marker() {
        let text = "workflow_id=e2e-tool-batch-failover tool_batch=true fail_once=true";

        assert_eq!(latest_fail_once_marker(text), Some(true));
    }

    #[test]
    fn scenario_marker_does_not_treat_fail_once_as_kitchen_sink() {
        let text = "workflow_id=e2e-tool-batch tool_batch=true fail_once=false";

        assert_eq!(latest_scenario_marker(text), Some(MockScenario::ToolBatch));
    }

    #[test]
    fn scenario_marker_detects_parent_durable_input_after_child() {
        let text = "workflow_id=e2e-parent-durable parent_durable_input_after_child=true";

        assert_eq!(
            latest_scenario_marker(text),
            Some(MockScenario::ParentDurableInputAfterChild)
        );
    }

    #[test]
    fn mock_scripts_use_paired_lashlang_tags() {
        let scripts = [
            kitchen_sink_script("e2e-test", false),
            kitchen_sink_script("e2e-test", true),
            trigger_setup_script(),
            signal_suspend_script("e2e-test"),
            queued_wake_script(),
            async_completion_script("e2e-test"),
            durable_input_request_script("e2e-test"),
            parent_durable_input_after_child_script("e2e-test"),
            tool_batch_script("e2e-test", false),
            tool_batch_script("e2e-test", true),
            segment_loop_script("e2e-test"),
        ];

        for script in scripts {
            assert!(
                !script.contains("```lashlang"),
                "mock script still uses markdown-fenced Lashlang:\n{script}"
            );
            assert_eq!(script.matches("<lashlang>").count(), 1);
            assert_eq!(script.matches("</lashlang>").count(), 1);
        }
    }
}
