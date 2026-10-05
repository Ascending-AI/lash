use super::*;
use lash_core_execution::{TriggerCommand, TriggerStore as _};

fn register_command(owner: &str, key: &str, source_type: &'static str) -> TriggerCommand {
    let source_key = lash_core_execution::facade_support::empty_trigger_source_key(source_type)
        .expect("source key");
    TriggerCommand::Register {
        owner_scope: lash_core_execution::TriggerOwnerScope::session(
            lash_core::SessionId::fixture(owner.to_string()),
        ),
        actor: lash_core_execution::ProcessOriginator::session(
            lash_core_execution::SessionScope::new(lash_core::SessionId::fixture(
                owner.to_string(),
            )),
        ),
        draft: lash_core_execution::TriggerSubscriptionDraft::for_process(
            key,
            lash_core_execution::ProcessExecutionEnvRef::new(format!(
                "process-env:fixture-{owner}"
            )),
            source_type,
            source_key,
            lash_core_execution::ProcessInput::Engine {
                kind: "test".to_string(),
                payload: serde_json::json!({ "owner": owner }),
            },
            lash_core_execution::ProcessIdentity::new("test"),
        )
        .with_payload_schema(lash_core_execution::JsonSchema::any()),
    }
}

fn column_i64(path: &Path, sql: &str, id: &str) -> i64 {
    let conn = rusqlite::Connection::open(path).expect("open raw trigger db");
    conn.query_row(sql, rusqlite::params![id], |row| row.get::<_, i64>(0))
        .expect("read revision column")
}

fn receipt_of(
    outcome: lash_core_execution::TriggerCommandOutcome,
) -> Box<lash_core_execution::TriggerMutationReceipt> {
    match outcome {
        lash_core_execution::TriggerCommandOutcome::Mutation { receipt } => receipt,
        other => panic!("expected a mutation receipt, got {other:?}"),
    }
}

#[tokio::test]
async fn trigger_revision_columns_carry_the_record_revision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trigger-revision-columns.db");
    let source_type = "ui.button.pressed";
    let store = SqliteTriggerStore::open(&path)
        .await
        .expect("open trigger store");

    let registered = receipt_of(
        store
            .execute_command(
                "register",
                register_command("owner", "counter-key", source_type),
            )
            .await
            .expect("execute registration")
            .expect("register row"),
    );
    let subscription_id = registered.record.subscription_id.clone();
    let registered_revision = registered.record.revision;

    // A fresh subscription has a real, positive revision, and the column
    // holds exactly it -- not a sentinel and not a constant.
    assert!(
        registered_revision > 0,
        "a fresh subscription revision must be positive, got {registered_revision}"
    );
    let stored = column_i64(
        &path,
        "SELECT revision FROM trigger_subscriptions WHERE subscription_id = ?1",
        subscription_id.as_str(),
    );
    assert_ne!(stored, -1, "the stored revision must not be a sentinel");
    assert_eq!(
        stored,
        i64::try_from(registered_revision).expect("revision fits i64"),
        "trigger_subscriptions.revision must equal the record revision"
    );

    // The delivery row copies the same counter through the same helper.
    let source_key = lash_core_execution::facade_support::empty_trigger_source_key(source_type)
        .expect("source key");
    let ingress = store
        .ingest_occurrence(lash_core_execution::TriggerOccurrenceRequest::new(
            source_type,
            source_key,
            serde_json::json!({ "button": "Blue" }),
            "revision-column-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let delivered = column_i64(
        &path,
        "SELECT subscription_revision FROM trigger_deliveries WHERE subscription_id = ?1",
        subscription_id.as_str(),
    );
    assert_ne!(
        delivered, -1,
        "the stored delivery revision must not be a sentinel"
    );
    assert_eq!(
        delivered,
        i64::try_from(registered_revision).expect("revision fits i64"),
        "trigger_deliveries.subscription_revision must equal the subscription revision"
    );

    // A mutation advances the counter, and the column follows it.
    let disabled = receipt_of(
        store
            .execute_command(
                "disable",
                TriggerCommand::Disable {
                    owner_scope: lash_core_execution::TriggerOwnerScope::session("owner"),
                    actor: lash_core_execution::ProcessOriginator::session(
                        lash_core_execution::SessionScope::new("owner"),
                    ),
                    subscription_key: "counter-key".to_string(),
                    expected_revision: registered_revision,
                },
            )
            .await
            .expect("execute disable")
            .expect("disable row"),
    );
    let disabled_revision = disabled.record.revision;
    assert_eq!(
        disabled_revision,
        registered_revision + 1,
        "a mutation advances the subscription revision"
    );
    let stored_after = column_i64(
        &path,
        "SELECT revision FROM trigger_subscriptions WHERE subscription_id = ?1",
        subscription_id.as_str(),
    );
    assert_ne!(
        stored_after, -1,
        "the stored revision must not be a sentinel"
    );
    assert_eq!(
        stored_after,
        i64::try_from(disabled_revision).expect("revision fits i64"),
        "trigger_subscriptions.revision must track the advanced record revision"
    );
    assert_ne!(
        stored_after, stored,
        "the column must move when the counter moves"
    );
}

#[tokio::test]
async fn subscription_changes_are_durable_on_sqlite_memory() {
    let store = crate::SqliteStoreSet::memory()
        .await
        .expect("memory stores")
        .trigger_store();
    let registered = receipt_of(
        store
            .execute_command(
                "change-register",
                register_command("change-owner", "change-key", "ui.button.pressed"),
            )
            .await
            .expect("store registration")
            .expect("registration"),
    );
    let changed = store
        .conn
        .call(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM trigger_subscription_changes WHERE change_seq > 0",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .expect("subscription mutations publish their durable change in the accepting transaction");
    assert_eq!(changed, 1);
    let (changes, cursor) = store
        .subscriptions_changed_since(
            lash_core_execution::TriggerSubscriptionChangeCursor::initial(),
            10,
        )
        .await
        .expect("change page");
    assert_eq!(
        changes,
        vec![lash_core_execution::TriggerSubscriptionChange::from(
            &registered.record
        )]
    );
    assert!(cursor.store_sequence() > 0);
}
