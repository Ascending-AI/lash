use super::*;
use lash_core_execution::{ProcessInput, ProcessProvenance, ProcessRegistration, RecoveryContract};

const REGISTERED: usize = 10;

/// Registers minted rows and pages the worklist two at a time: the pages must
/// visit every row once, in byte order of the minted id, on either backend.
async fn registered_and_paged_ids(registry: &dyn ProcessRegistry) -> (Vec<String>, Vec<String>) {
    let mut registered = Vec::new();
    for _ in 0..REGISTERED {
        registered.push(
            registry
                .register_process(ProcessRegistration::new(
                    ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    RecoveryContract::ExternallyOwned,
                    ProcessProvenance::host(),
                    lash_core_execution::Lifetime::Detached,
                ))
                .await
                .expect("register worklist fixture")
                .id
                .to_string(),
        );
    }
    let mut ids = Vec::new();
    let mut continuation = None;
    loop {
        let page = registry
            .list_non_terminal_page(std::num::NonZeroUsize::new(2).unwrap(), continuation)
            .await
            .expect("read worklist page");
        ids.extend(page.records.into_iter().map(|record| record.id.to_string()));
        continuation = page.continuation;
        if continuation.is_none() {
            return (registered, ids);
        }
        assert!(ids.len() <= REGISTERED, "pagination must advance");
    }
}

#[tokio::test]
async fn worklist_pagination_is_byte_ordered_on_both_backends() {
    let Some((_database_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let sqlite = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite registry")
        .process_registry();
    for (name, registry) in [
        ("sqlite", sqlite.as_ref() as &dyn ProcessRegistry),
        (
            "postgres",
            &storage.process_registry() as &dyn ProcessRegistry,
        ),
    ] {
        let (mut registered, paged) = registered_and_paged_ids(registry).await;
        registered.sort_unstable();
        assert_eq!(paged, registered, "{name} worklist byte order");
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
    // The parent-end ledger is keyed by scope, not by a process row, so its
    // identifier column is `parent_id`. It is paged in byte order by the same
    // sweep, so it carries the same collation requirement.
    let ledger: String = sqlx::query_scalar(
        "SELECT c.collname FROM pg_attribute a JOIN pg_collation c ON c.oid = a.attcollation
         WHERE a.attrelid = 'lash_parent_end_plans'::regclass AND a.attname = 'parent_id'",
    )
    .fetch_one(storage.pool())
    .await
    .expect("parent-end ledger column collation");
    assert_eq!(
        ledger, "C",
        "lash_parent_end_plans parent identifiers require byte order"
    );
    let index: String = sqlx::query_scalar(
        "SELECT c.collname FROM pg_index i JOIN pg_collation c ON c.oid = i.indcollation[0]
         WHERE i.indexrelid = 'idx_lash_processes_live_worklist'::regclass",
    )
    .fetch_one(storage.pool())
    .await
    .expect("index collation");
    assert_eq!(index, "C", "worklist index must inherit byte order");
}
