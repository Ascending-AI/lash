use super::*;

fn completed_continue_as_effect_fixture() -> (RuntimeEffectEnvelope, RuntimeEffectOutcome) {
    let call_id = "continue-as-call";
    let envelope = RuntimeEffectEnvelope::new(
        lash_core::RuntimeInvocation::effect(
            lash_core::RuntimeScope::for_turn("cutover-session", "cutover-turn", 3, 1),
            "continue-as-attempt",
            RuntimeEffectKind::ToolAttempt,
            "continue-as-attempt-replay",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: lash_core::PreparedToolCall::from_parts(
                call_id,
                lash_core::ToolId::from("tool:continue_as"),
                "continue_as",
                serde_json::json!({ "task": "continue after redrive" }),
                None,
                serde_json::Value::Null,
            ),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let outcome = RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
            record: Box::new(lash_core::ToolCallRecord {
                call_id: Some(call_id.to_string()),
                tool: "continue_as".to_string(),
                args: serde_json::json!({ "task": "continue after redrive" }),
                output: lash_core::ToolCallOutput::success(serde_json::json!({ "ok": true }))
                    .with_control(lash_core::ToolControl::SwitchAgentFrame {
                        frame_key: lash_core::FrameKey::from_call_site(
                            "cutover-session",
                            "cutover-frame",
                            call_id,
                        ),
                        initial_nodes: Vec::new(),
                        task: Some("continue after redrive".to_string()),
                    }),
                duration_ms: 4,
            }),
            intents: lash_core::ToolIntents::v1(Vec::new()),
        }),
        triggers: Vec::new(),
    };
    (envelope, outcome)
}

fn rewrite_completed_continue_as_outcome_to_frame_id(outcome_json: &str) -> String {
    let mut value: serde_json::Value =
        serde_json::from_str(outcome_json).expect("decode completed continue_as outcome");
    let control = value
        .pointer_mut("/launch/record/output/control")
        .and_then(serde_json::Value::as_object_mut)
        .expect("completed continue_as control");
    let frame_key = control
        .remove("frame_key")
        .expect("current fixture carries frame_key");
    control.insert("frame_id".to_string(), frame_key);
    serde_json::to_string(&value).expect("encode pre-cutover continue_as outcome")
}

#[tokio::test]
async fn sqlite_refuses_completed_pre_frame_key_continue_as_at_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-frame-key-continue-as.db");
    let (envelope, outcome) = completed_continue_as_effect_fixture();
    let controller = SqliteRuntimeEffectController::open(
        &path,
        durable_turn_scope("cutover-session", "cutover-turn"),
    )
    .await
    .expect("create current effect store");
    controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move { Ok(outcome) }),
        )
        .await
        .expect("journal completed continue_as");
    drop(controller);

    let conn = rusqlite::Connection::open(&path).expect("open raw effect store");
    let outcome_json: String = conn
        .query_row(
            "SELECT outcome_json FROM runtime_effect_replay WHERE replay_key = ?1",
            rusqlite::params!["continue-as-attempt-replay"],
            |row| row.get(0),
        )
        .expect("read completed continue_as outcome");
    let legacy_outcome = rewrite_completed_continue_as_outcome_to_frame_id(&outcome_json);
    assert!(legacy_outcome.contains("\"frame_id\""));
    assert!(!legacy_outcome.contains("\"frame_key\""));
    conn.execute(
        "UPDATE runtime_effect_replay SET outcome_json = ?1 WHERE replay_key = ?2",
        rusqlite::params![legacy_outcome, "continue-as-attempt-replay"],
    )
    .expect("install completed pre-cutover continue_as outcome");
    conn.pragma_update(None, "user_version", 8)
        .expect("stamp pre-frame-key effect schema");
    drop(conn);

    let error = match SqliteRuntimeEffectController::open(
        &path,
        durable_turn_scope("cutover-session", "cutover-turn"),
    )
    .await
    {
        Ok(_) => panic!("pre-frame-key journal must be refused at open"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "Error(\"Unsupported lash effect replay schema: this binary supports schema version 10, but the database reports version 8. There is no migration chain — drain affected sessions and recreate the whole Lash trust domain with this version. Reset the tombstones, await-event revocation ledger, effect journal, and Restate state together; see docs/persistence.html#delete-sessions.\")"
    );
}
