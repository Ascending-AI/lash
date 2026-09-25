use super::*;

fn event(kind: BoundaryKind, id: &str, payload: Value) -> BoundaryEvent {
    BoundaryEvent::new(id, "session-001", kind, 1, "test", payload)
}

/// A harness over a fresh engine on the server double.
async fn harness() -> RuntimeBoundaryHarness {
    RuntimeBoundaryHarness::new(
        crate::backend::SimEngine::new(0x5eed_7020)
            .await
            .expect("sim engine"),
    )
}

#[tokio::test]
async fn durable_effect_redrive_is_served_the_recorded_result() {
    let mut harness = harness().await;
    let observed = harness
        .complete_durable_effect(&event(
            BoundaryKind::DurableEffect,
            "durable:001",
            json!({
                "durable_key": "sleep/session-001/001",
                "result": {"completed": true},
                "redrive_result": {"completed": false},
                "runtime_effect": {"effect_id": "effect/sleep/001"},
            }),
        ))
        .await
        .expect("durable effect under crash and redrive");

    assert_eq!(observed["execution_count"], 1);
    assert_eq!(observed["replay_count"], 1);
    assert_eq!(observed["replayed"], true);
    assert_eq!(observed["redrive_served_recorded_result"], true);
    assert_eq!(observed["result_digest"], observed["redrive_result_digest"]);
    assert_eq!(observed["runtime_effect"]["local_executor_called"], true);
    assert_eq!(
        observed["runtime_effect"]["redrive_local_executor_called"],
        false
    );
    assert_eq!(
        observed["runtime_effect"]["controller"],
        RUNTIME_EFFECT_CONTROLLER
    );
}

#[tokio::test]
async fn tool_boundary_runs_on_the_handler_controller_and_records_output() {
    let mut harness = harness().await;
    let observed = harness
        .complete_tool(&event(
            BoundaryKind::Tool,
            "tool:001",
            json!({
                "tool": "lookup",
                "output": {"answer": "tool data"},
            }),
        ))
        .await
        .expect("tool boundary");

    assert_eq!(observed["execution_count"], 1);
    assert_eq!(
        observed["runtime_effect"]["controller"],
        RUNTIME_EFFECT_CONTROLLER
    );
    assert_eq!(observed["runtime_tool_record"]["tool"], "lookup");
    assert!(
        observed["runtime_tool_output"]
            .to_string()
            .contains("tool data")
    );
}

#[tokio::test]
async fn exec_boundary_runs_on_the_handler_controller_and_preserves_exit_data() {
    let mut harness = harness().await;
    let observed = harness
        .execute_code(&event(
            BoundaryKind::ExecCode,
            "exec:001",
            json!({
                "output": "exec data",
                "exit_code": 7,
            }),
        ))
        .await
        .expect("exec boundary");

    assert_eq!(observed["execution_count"], 1);
    assert_eq!(
        observed["runtime_effect"]["controller"],
        RUNTIME_EFFECT_CONTROLLER
    );
    assert_eq!(observed["exit_code"], 7);
    assert!(
        observed["runtime_effect_outcome"]
            .to_string()
            .contains("exec data")
    );
}

#[tokio::test]
async fn exec_boundary_matches_model_replay_projection() {
    let boundary = event(
        BoundaryKind::ExecCode,
        "session-001:exec-code:001",
        json!({
            "output": "exec result 1 for session-001",
            "exit_code": 0,
        }),
    );
    let mut live_harness = harness().await;
    let live = live_harness
        .execute_code(&boundary)
        .await
        .expect("live exec boundary");
    let replay = crate::store::ModelStore::default().project_boundary_observation(&boundary);

    assert_eq!(
        live, replay,
        "exec-code runtime truth and model replay projection diverged"
    );
    assert!(
        live.get("runtime_effect_outcome").is_some(),
        "the exact boundary payload must retain the settled outcome"
    );
}
