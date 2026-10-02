use super::*;
use lash_core_execution::{TriggerOccurrenceOutcome, TriggerOccurrenceRequest, TriggerStore as _};

async fn retention_uses_columns(store: Arc<SqliteTriggerStore>) {
    for (key, outcome) in [
        ("fired", TriggerOccurrenceOutcome::Fired),
        (
            "dropped",
            TriggerOccurrenceOutcome::Dropped {
                reason: "audit".into(),
            },
        ),
    ] {
        store
            .ingest_occurrence(
                TriggerOccurrenceRequest::new("source", "key", serde_json::json!({}), key)
                    .with_outcome(outcome),
            )
            .await
            .expect("ingest occurrence");
    }
    store
        .conn
        .write(|tx| tx.execute("UPDATE trigger_occurrences SET record_json = '{broken'", []))
        .await
        .expect("corrupt presentation bytes");
    let report = store
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("reclaim must never decode record_json");
    assert_eq!(report.reclaimed_occurrence_count, 1);
    assert_eq!(report.audit_retained_count, 1);
    assert_eq!(
        store
            .prune_non_fired_occurrences(u64::MAX)
            .await
            .expect("prune must never decode record_json"),
        1
    );
    let tombstones = store
        .conn
        .call(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM trigger_occurrence_tombstones",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .expect("tombstone count");
    assert_eq!(tombstones, 2);
}

#[tokio::test]
async fn retention_uses_typed_outcomes_on_sqlite_memory() {
    retention_uses_columns(
        crate::SqliteStoreSet::memory()
            .await
            .expect("memory store set")
            .trigger_store(),
    )
    .await;
}

#[tokio::test]
async fn retention_uses_typed_outcomes_on_disk() {
    let dir = tempfile::tempdir().expect("trigger directory");
    retention_uses_columns(Arc::new(
        SqliteTriggerStore::open(&dir.path().join("triggers.db"))
            .await
            .expect("file trigger store"),
    ))
    .await;
}

#[tokio::test]
async fn dropped_occurrences_cannot_be_reclaimed_or_have_deliveries() {
    let store = crate::SqliteStoreSet::memory()
        .await
        .expect("memory store set")
        .trigger_store();
    let record = store
        .ingest_occurrence(
            TriggerOccurrenceRequest::new("source", "key", serde_json::json!({}), "dropped")
                .with_outcome(TriggerOccurrenceOutcome::Dropped {
                    reason: "audit".into(),
                }),
        )
        .await
        .expect("dropped occurrence")
        .occurrence;
    store.conn.write(move |tx| {
        assert!(tx.execute("UPDATE trigger_occurrences SET reclaimable_at_ms = 0 WHERE occurrence_id = ?1", params![record.occurrence_id]).is_err(), "dropped rows cannot arm reclamation");
        assert!(tx.execute("INSERT INTO trigger_deliveries (occurrence_id, subscription_id, subscription_incarnation, subscription_revision, subscription_snapshot_json, created_at_ms) VALUES (?1, 'sub', 'incarnation', 1, '{}', 0)", params![record.occurrence_id]).is_err(), "dropped rows cannot reserve a delivery");
        assert!(tx.execute("UPDATE trigger_occurrences SET outcome_kind = 'unknown'", []).is_err(), "outcome vocabulary is closed");
        Ok(())
    }).await.expect("schema rejects impossible states");
}
