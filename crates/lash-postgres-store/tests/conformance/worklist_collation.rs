use super::*;
use lash_core::{
    ProcessInput, ProcessProvenance, ProcessRegistration, RecoveryContract,
    TestLocalProcessRegistry,
};

// Literal byte order deliberately differs from en_US.utf8 punctuation handling.
const IDS: [&str; 10] = ["!!a", "!z", "-a", "0", "A", "_a", "a", "a!", "a-", "~a"];

async fn ordered_ids(registry: &dyn ProcessRegistry) -> Vec<String> {
    for id in IDS.into_iter().rev() {
        registry
            .register_process(ProcessRegistration::new(
                id,
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ))
            .await
            .expect("register punctuation fixture");
    }
    let mut ids = Vec::new();
    let mut continuation = None;
    loop {
        let page = registry
            .list_non_terminal_page(std::num::NonZeroUsize::new(2).unwrap(), continuation)
            .await
            .expect("read punctuation page");
        if let Some(cursor) = &page.continuation {
            assert_eq!(
                cursor.through_process_id(),
                "~a",
                "maximum must use byte order"
            );
        }
        ids.extend(page.records.into_iter().map(|record| record.id.to_string()));
        continuation = page.continuation;
        if continuation.is_none() {
            return ids;
        }
        assert!(ids.len() <= IDS.len(), "pagination must advance");
    }
}

#[tokio::test]
async fn punctuation_worklist_pagination_matches_all_three_backends() {
    let Some((_database_lock, storage)) = storage().await else {
        return;
    };
    reset(&storage).await;
    let memory = TestLocalProcessRegistry::default();
    let sqlite = lash_sqlite_store::SqliteProcessRegistry::memory()
        .await
        .expect("SQLite registry");
    for (name, registry) in [
        ("memory", &memory as &dyn ProcessRegistry),
        ("sqlite", &sqlite as &dyn ProcessRegistry),
        (
            "postgres",
            &storage.process_registry() as &dyn ProcessRegistry,
        ),
    ] {
        assert_eq!(
            ordered_ids(registry).await,
            IDS,
            "{name} worklist byte order"
        );
    }
}

#[tokio::test]
async fn process_family_columns_and_worklist_index_pin_c_collation() {
    let Some((_database_lock, storage)) = storage().await else {
        return;
    };
    for table in [
        "lash_processes",
        "lash_process_events",
        "lash_wake_allocation_floors",
        "lash_process_wake_deliveries",
        "lash_process_observers",
        "lash_process_tombstones",
        "lash_process_leases",
        "lash_process_segment_handovers",
        "lash_process_parent_end_plans",
    ] {
        let collation: String = sqlx::query_scalar(
            "SELECT c.collname FROM pg_attribute a JOIN pg_collation c ON c.oid = a.attcollation
             WHERE a.attrelid = $1::regclass AND a.attname = 'process_id'",
        )
        .bind(table)
        .fetch_one(storage.pool())
        .await
        .expect("process-family column collation");
        assert_eq!(
            collation, "C",
            "{table} process identifiers require byte order"
        );
    }
    let index: String = sqlx::query_scalar(
        "SELECT c.collname FROM pg_index i JOIN pg_collation c ON c.oid = i.indcollation[0]
         WHERE i.indexrelid = 'idx_lash_processes_live_worklist'::regclass",
    )
    .fetch_one(storage.pool())
    .await
    .expect("index collation");
    assert_eq!(index, "C", "worklist index must inherit byte order");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_registry_pagination_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres pagination conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    lash_conformance::process_registry_pagination(registry).await;
}
