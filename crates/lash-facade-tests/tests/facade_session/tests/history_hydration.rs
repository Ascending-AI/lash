use super::*;
// A persistent session's sends decode what they add, not what came before
// (FIG-5930).
//
// A send reads its session's window several times: the engine's turn, the
// runtime its pass opens, the commit's publication, the open session's
// refresh and the handle's terminal report. The store reuses the window it
// decoded and reads only the rows past its leaf, so the node bodies a send
// decodes are bounded by the nodes it appends, whatever the session's
// history depth.

use lash::TurnInput;
use lash_core::store::StoreTestSupport;
use lash_facade_tests::history::*;
use lash_kernel_value::identity::SessionId;
use std::time::Duration;

/// Each measured send decodes at most one body per window read for each
/// node it appends, so the total stays proportional to the nodes appended
/// while the window it reads grows by them on every send.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_decodes_only_the_nodes_it_appends() -> Result<()> {
    let files = tempfile::tempdir().expect("SQLite store directory");
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        files.path().join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .expect("SQLite file stores");
    let store = stores.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(Arc::new(stores)),
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let id = SessionId::fixture("history-hydration");
    core.session(id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(id.clone()).open().await?;
    for turn in 0..WARM_SENDS {
        session
            .send(TurnInput::text(format!("warm {turn}")))
            .output()
            .await?;
    }
    let depth_before = window_len(&store, &id).await;
    let mut decoded = 0;
    let mut read = 0;
    for turn in 0..MEASURED_SENDS {
        let before = store.decoded_row_counts_for_testing().graph_node_bodies;
        session
            .send(TurnInput::text(format!("turn {turn}")))
            .output()
            .await?;
        decoded += store.decoded_row_counts_for_testing().graph_node_bodies - before;
        read += window_len(&store, &id).await;
    }
    let appended = window_len(&store, &id).await - depth_before;
    assert!(
        appended >= MEASURED_SENDS as u64,
        "{appended} nodes appended"
    );
    assert!(
        decoded <= WINDOW_READS_PER_SEND * appended,
        "{MEASURED_SENDS} sends appended {appended} nodes and decoded {decoded} bodies \
         (windows of {read} nodes in all)"
    );
    session.close().await?;
    core.shutdown().await?;
    Ok(())
}

/// FIG-6208: the reader opened before the first turn still keeps its new
/// anchored window warm after the engine releases its idle head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_empty_session_keeps_its_first_window_warm_after_idle_eviction() -> Result<()> {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory stores");
    let store = stores.session_store_factory();
    let settings = lash::durable::DurableSettings {
        owner_retention: lash::durable::OwnerRetentionSettings {
            idle: Duration::from_millis(1),
            ..lash::durable::OwnerRetentionSettings::default()
        },
        ..Default::default()
    };
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .config(settings)
        .build()
        .expect("fixture durable backend");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(lash_facade_tests::runtime_lease_owner())?;
    let id = SessionId::fixture("empty-reader-idle-eviction");
    core.session(id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(id.clone()).open().await?;
    session.send(TurnInput::text("first turn")).output().await?;
    let actor = lash_core::durable_port::ActorKey::session(id.as_str()).expect("session actor");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = core
                .backend()
                .durable()
                .actor(&actor)
                .await
                .expect("read the session actor");
            if snapshot
                .is_some_and(|snapshot| snapshot.state == lash_core::durable_port::ActorState::Idle)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the engine releases its idle head");
    let before = store.decoded_row_counts_for_testing().graph_node_bodies;
    assert!(
        window_len(&store, &id).await > 1,
        "the first turn anchored the window"
    );
    assert_eq!(
        store.decoded_row_counts_for_testing().graph_node_bodies - before,
        0,
        "the open empty reader keeps the first anchored window warm"
    );
    session.close().await?;
    core.shutdown().await?;
    Ok(())
}
