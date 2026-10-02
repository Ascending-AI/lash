//! Engine-control refusals over SQLite files and the Restate double.
use super::*;
use crate::tests::{
    conformance_and_poison::external_registration,
    effect_group_committed_recovery::HarnessStoreTier,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_usage_retirement_keeps_a_terminal_request_refused_after_one_attempt() {
    let harness = LiveConformanceHarness::start_for_tool_children_over(
        HarnessServer::in_process(),
        HarnessStoreTier::SqliteFile,
    )
    .await;
    let f = Fixture::from_harness(harness, false, false).await;
    f.reconcile_until(false).await;
    let park = f
        .driver
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("parked root");
    let intent = f
        .factory
        .open_root_intent(
            &RootIntentRequest {
                session_id: f.driver.session.clone(),
                root: f.driver.root.clone(),
                park: park.park_id,
                verb: RootVerb::Cancel,
            },
            5,
        )
        .await
        .expect("cancel intent");
    // The accounting ingress has an unbound route. Its request receives
    // a terminal 404 from the double, after the normal release lookup.
    let stores = f.harness.law_stores();
    let control = Arc::new(crate::session_control::RestateSessionControl {
        lost_processes: Default::default(),
        lost_roots: Default::default(),
        admin: f.harness.admin_client(),
        ingress: crate::RestateIngressClient::new(crate::RestateConnection::with_transport(
            format!(
                "{}/unbound-retirement",
                f.harness.connection().ingress_url()
            ),
            f.harness.server_double().expect("double").transport(),
        )),
        namespace: crate::RestateNamespace::default(),
        processes: stores.process_registry(),
        continuations: stores.process_continuations(),
        generation: EngineGeneration::fixed(BuildGeneration::for_test("effect-group-conformance")),
        sessions: Arc::clone(&f.factory),
    });
    let state = f
        .relay(Some(control), Arc::new(NoScopeClose))
        .deliver_intent(&intent)
        .await
        .expect("delivery");
    assert!(
        matches!(&state, ControlIntentState::Refused { cause }
        if cause.code == lash_core::RuntimeErrorCode::EngineControlRequest),
        "{state:?}"
    );
    let stalls = f
        .intents
        .list_stalled(None, NonZeroUsize::MIN)
        .await
        .expect("stalls");
    assert_eq!(stalls.len(), 1);
    assert_eq!(stalls[0].attempts, 1);
    assert_eq!(stalls[0].reason, StallReason::Refused);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_registry_read_is_refused_identically_by_store_and_engine_control() {
    let harness = LiveConformanceHarness::start_for_tool_children_over(
        HarnessServer::in_process(),
        HarnessStoreTier::SqliteFile,
    )
    .await;
    let registry = harness.law_stores().process_registry();
    let record = registry
        .register_process(external_registration())
        .await
        .expect("process");
    let connection = rusqlite::Connection::open(
        harness.sqlite_database_uri(lash_sqlite_store::SqliteDatabase::ProcessRegistry),
    )
    .expect("SQL observer");
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
