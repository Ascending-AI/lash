async fn assert_typed_turn_input_application(
    session: &lash::LashSession,
    rx: &mut mpsc::Receiver<ObservationStreamItem>,
) {
    let admission = session
        .enqueue(lash::TurnInput::text("queued workbench input"))
        .id("workbench-queued-input")
        .send()
        .await
        .expect("admit queued workbench input");
    session
        .queued_turn()
        .drain_id("workbench-queued-turn")
        .run()
        .await
        .expect("drain queued workbench input")
        .expect("queued workbench turn");
    let mut typed_application = None;
    for _ in 0..64 {
        let item = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for typed application")
            .expect("stream item");
        let ObservationStreamItem::Observation { event } = item else {
            continue;
        };
        let value = serde_json::to_value(&event).expect("remote event json");
        if value.pointer("/type").and_then(Value::as_str) == Some("turn_activity")
            && value.pointer("/activity/type").and_then(Value::as_str)
                == Some("turn_input_applied")
        {
            assert!(
                value.pointer("/activity/kind").is_none(),
                "workbench must consume a typed event, not RuntimeDiagnostic: {value}"
            );
            typed_application = value.pointer("/activity/applications/0").cloned();
            break;
        }
    }
    let typed_application = typed_application.expect("typed application observation");
    assert_eq!(
        typed_application.get("input_id").and_then(Value::as_str),
        Some(admission.input_id.as_str())
    );
    assert_eq!(
        typed_application.get("source_key").and_then(Value::as_str),
        Some("host:workbench-queued-input")
    );
    assert_eq!(
        typed_application.get("turn_id").and_then(Value::as_str),
        Some("workbench-queued-turn")
    );
    let durable = session
        .remote_turn_input_applications()
        .await
        .expect("durable workbench applications");
    // The turn that drained this admission was itself admitted first
    // (ADR 0069), so the session settles that acceptance too; what this test
    // pins is the queued admission's own settled evidence.
    let settled = durable
        .iter()
        .find(|application| application.input_id == admission.input_id)
        .expect("the queued admission settles as durable application evidence");
    assert_eq!(settled.turn_id.as_str(), "workbench-queued-turn");
    assert!(
        session
            .pending_turn_inputs()
            .await
            .expect("pending input snapshot")
            .is_empty(),
        "application evidence must remain available without a pending-input snapshot"
    );
    assert!(ui::INDEX_HTML.contains("turn_input_applied"));
    assert!(!ui::INDEX_HTML.contains("queued_input_accepted"));
    assert!(!ui::INDEX_HTML.contains("runtime_diagnostic"));
}
