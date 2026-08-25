use lash_core::{
    ExecutionScope, RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_postgres_store::PostgresStorage;

use crate::support::{SharedDatabaseLock, database_url};

async fn storage() -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let url = database_url()?;
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect postgres");
    Some((database_lock, storage))
}

async fn reset(storage: &PostgresStorage) {
    let pool = storage.pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("list lash_* fixture tables");
    assert!(!tables.is_empty(), "expected provisioned Lash tables");
    let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
    sqlx::query(&truncate)
        .execute(pool)
        .await
        .expect("reset Postgres cutover fixture tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(pool)
    .await
    .expect("reset Postgres process change clock");
}

fn completed_continue_as_effect_fixture() -> (lash_core::RuntimeEffectEnvelope, RuntimeEffectOutcome)
{
    let call_id = "continue-as-call";
    let envelope = lash_core::RuntimeEffectEnvelope::new(
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_refuses_completed_pre_frame_key_continue_as_at_open_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres pre-frame-key open gate: database is not configured");
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    let controller =
        storage.runtime_effect_controller(ExecutionScope::turn("cutover-session", "cutover-turn"));
    let (envelope, outcome) = completed_continue_as_effect_fixture();
    controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move { Ok(outcome) }),
        )
        .await
        .expect("journal completed continue_as");

    let outcome_json: String = sqlx::query_scalar(
        "SELECT outcome_json FROM lash_runtime_effect_replay WHERE replay_key = $1",
    )
    .bind("continue-as-attempt-replay")
    .fetch_one(&pool)
    .await
    .expect("read completed continue_as outcome");
    let legacy_outcome = rewrite_completed_continue_as_outcome_to_frame_id(&outcome_json);
    assert!(legacy_outcome.contains("\"frame_id\""));
    assert!(!legacy_outcome.contains("\"frame_key\""));
    sqlx::query("UPDATE lash_runtime_effect_replay SET outcome_json = $1 WHERE replay_key = $2")
        .bind(legacy_outcome)
        .bind("continue-as-attempt-replay")
        .execute(&pool)
        .await
        .expect("install completed pre-cutover continue_as outcome");
    sqlx::query(
        "UPDATE lash_schema_versions SET version = 43 WHERE component = 'lash-postgres-store'",
    )
    .execute(&pool)
    .await
    .expect("stamp pre-frame-key component schema");

    let result = PostgresStorage::from_pool(pool.clone()).await;

    sqlx::query(
        "UPDATE lash_schema_versions SET version = 59 WHERE component = 'lash-postgres-store'",
    )
    .execute(&pool)
    .await
    .expect("restore current component schema");
    sqlx::query("DELETE FROM lash_runtime_effect_replay WHERE replay_key = $1")
        .bind("continue-as-attempt-replay")
        .execute(&pool)
        .await
        .expect("remove pre-cutover fixture");

    let message = match result {
        Ok(_) => panic!("pre-frame-key journal must be refused at open"),
        Err(error) => error.to_string(),
    };
    assert_eq!(
        message,
        "store backend error: Postgres schema component `lash-postgres-store` has version 43, expected 59. The component schema is normally a reject-and-recreate boundary. This build has explicit Lash-managed migrations from the published component-50, component-51, component-52, component-53, component-54, component-55, component-56, and component-57 shapes to 59; they run only under SchemaCheck::Enforce after an exact source-shape preflight. This mismatch has no applicable migration. Drain affected sessions and recreate the whole Lash trust domain with this version: provision the database from this build's schema.sql artifact, and reset the tombstones, await-event revocation ledger, effect journal, and Restate state together; see docs/persistence.html#delete-sessions. This gate is unconditional; SchemaCheck::WarnOnly does not relax it."
    );
}
