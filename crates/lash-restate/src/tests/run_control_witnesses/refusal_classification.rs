//! Engine-control refusals over SQLite files and the Restate double.
use super::*;
use crate::tests::{
    conformance_and_poison::held_registration, harness_store_tiers::HarnessStoreTier,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_registry_read_is_refused_identically_by_store_and_engine_control() {
    let harness = LiveConformanceHarness::start_for_tools_over(
        HarnessServer::in_process(),
        HarnessStoreTier::SqliteFile,
    )
    .await;
    let registry = harness.law_stores().process_registry();
    let record = registry
        .register_process(held_registration())
        .await
        .expect("process");
    let connection =
        rusqlite::Connection::open(harness.sqlite_database_uri()).expect("SQL observer");
    assert_eq!(
        connection
            .execute(
                "UPDATE processes SET record_json = '{' WHERE process_id = ?1",
                [record.id.as_str()],
            )
            .expect("corrupt registry row"),
        1
    );
    let plugin = registry
        .get_process(&record.id)
        .await
        .expect_err("corrupt read");
    assert!(
        matches!(&plugin, lash_core::PluginError::StoredDataCorrupt { record_kind, .. }
        if record_kind == "process_registry")
    );
    let expected = EngineRefusal::from(plugin);
    let actual = harness
        .session_work()
        .control()
        .resume_process(&record.id, ParkId::from_feed_sequence(1))
        .await
        .expect_err("corrupt engine read");
    assert_eq!(actual, expected);
    assert_eq!(actual.disposition, RefusalClass::Permanent);
    assert_eq!(
        actual.code,
        lash_core::RuntimeErrorCode::RuntimeStoreCorrupt
    );
    harness.finish().await;
}
