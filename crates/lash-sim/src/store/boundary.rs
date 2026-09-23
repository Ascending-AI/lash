//! Scheduler-boundary helpers for the abstract model store: session aliasing and
//! the suspend-boundary projection the cross-backend replay compares against.

use serde_json::{Value, json};

use crate::scheduler::{BoundaryEvent, BoundaryKind};

pub(super) fn boundary_session_alias(event: &BoundaryEvent) -> String {
    event
        .payload
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or(&event.actor_alias)
        .to_string()
}

/// A suspend-session ingress or its scheduler-delivered resume completion.
pub(super) fn is_suspend_boundary(event: &BoundaryEvent) -> bool {
    (event.kind == BoundaryKind::Ingress && event.payload.get("suspend_kind").is_some())
        || event
            .payload
            .get("suspend_resume")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// Project the abstract observed for a suspend boundary, matching the
/// generated-world observed so cross-backend replay stays equal without
/// modelling the suspend session as a real abstract session.
pub(super) fn project_suspend_boundary(event: &BoundaryEvent) -> Option<Value> {
    if event.kind == BoundaryKind::Ingress && event.payload.get("suspend_kind").is_some() {
        return Some(json!({
            "session": event.actor_alias,
            "opened": true,
            "ingress_count": 1,
            "runtime_suspend": {
                "suspend_kind": event.payload.get("suspend_kind").cloned().unwrap_or(Value::Null),
                "spawned": true,
            },
        }));
    }
    if event
        .payload
        .get("suspend_resume")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let output = event
            .payload
            .get("output")
            .cloned()
            .unwrap_or_else(|| json!(""));
        let tool_name = event
            .payload
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("await_tool");
        let suspend_kind = match event.kind {
            BoundaryKind::Tool => "tool",
            BoundaryKind::ExecCode => "exec_code",
            BoundaryKind::DurableEffect => "durable_effect",
            _ => "unknown",
        };
        return Some(json!({
            "session": event.actor_alias,
            "tool_output": output,
            "tool_name": tool_name,
            "tool_call_id": event.boundary_id,
            "execution_count": 1,
            "runtime_tool_output": lash_core::ToolCallOutput::success(output.clone()),
            "runtime_suspend": {
                "suspend_kind": suspend_kind,
                "turn_suspended_before_completion": true,
                "scheduler_delivered_completion": true,
                "resolve_accepted": true,
                "resumed_after_completion": true,
                "completed_event_count_before_resolution": 0,
                "completed_event_count_after_resolution": 1,
                "final_assistant_message": "resumed",
            },
        }));
    }
    None
}
