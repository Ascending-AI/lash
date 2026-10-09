//! FIG-5549: a commit to a process's log on one core ticks the process
//! change hub of every other core on the same store, through an every-node
//! `appended` node wake, so a wait on one core never depends on a trace or
//! a poll of its own to learn what another core committed.

use super::*;

use std::time::Duration;

use lash_core::StoreSet as _;

/// One node of a SQLite database file: a core of its own over a store set
/// of its own.
async fn core_on(
    database: &std::path::Path,
    node: &str,
) -> (Arc<lash_sqlite_store::SqliteStoreSet>, LashCore) {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            database,
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open the file store set"),
    );
    let core = standard_core_builder_over(lash_conformance::backend_over(stores.clone()))
        .build(lash_core::LeaseOwnerIdentity::opaque(
            node,
            format!("{node}-boot"),
        ))
        .expect("standard core");
    (stores, core)
}

/// Two cores serve one SQLite file as two nodes. Core A watches a process
/// it executes nothing of; core B commits the process's terminal. A's
/// change hub ticks, with no timer and no trace on A, and A then reads the
/// terminal from the store.
#[tokio::test]
async fn a_commit_on_one_core_ticks_the_process_change_hub_of_another() {
    let dir = tempfile::tempdir().expect("two-core tempdir");
    let database = dir.path().join("lash.db");
    let (stores, core_a) = core_on(&database, "fig-5549-follower").await;
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let (_stores_b, core_b) = core_on(&database, "fig-5549-committer").await;
    // Both nodes listen before the commit: each holds its boot's liveness
    // lock once its listener is open.
    let node_wakes = stores.node_wakes().expect("a SQLite file has node wakes");
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let boots = node_wakes.liveness().await.expect("probe liveness");
            if boots.iter().filter(|boot| boot.held).count() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both nodes listen");

    let committer = core_b.process_registry.clone();
    let process_id = committer
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::testing::held_engine_input(serde_json::Value::Null),
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref())),
        )
        .await
        .expect("register on core B")
        .id;

    let mut changes = core_a.process_changes().subscribe(&process_id);
    let terminal = committer
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(&process_id),
        )
        .await
        .expect("complete on core B");
    tokio::time::timeout(Duration::from_secs(10), changes.changed())
        .await
        .expect("core A hears core B's commit")
        .expect("core A's hub lives");
    let seen = core_a
        .process_registry
        .clone()
        .get_process(&process_id)
        .await
        .expect("read on core A")
        .expect("the process is retained");
    assert_eq!(seen.last_event_sequence, terminal.last_event_sequence);
    assert!(seen.outcome().is_some(), "core A reads the terminal");
}
