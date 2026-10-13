//! A persistent session's sends decode what they add, not what came before
//! (FIG-5930).
//!
//! A send reads its session's window several times: the engine's turn, the
//! runtime its pass opens, the commit's publication, the open session's
//! refresh and the handle's terminal report. The store reuses the window it
//! decoded and reads only the rows past its leaf, so the node bodies a send
//! decodes are bounded by the nodes it appends, whatever the session's
//! history depth.

use super::*;
use crate::{SessionId, TurnInput};
use lash_core::store::{SessionHistoryStore, StoreTestSupport, WindowSelector};

/// The window reads one send makes: the engine's turn, the runtime its pass
/// opens, the commit's publication, the open session's refresh and the
/// handle's terminal report.
const WINDOW_READS_PER_SEND: u64 = 5;
/// Sends before the measured ones: the session's first turns open its
/// runtime and decode the window its creation wrote.
const WARM_SENDS: usize = 3;
const MEASURED_SENDS: usize = 24;

async fn window_len(store: &lash_sqlite_store::SqliteStore, session: &SessionId) -> u64 {
    store
        .load_session_window(session, WindowSelector::Current)
        .await
        .expect("read the window")
        .expect("the session has a window")
        .window
        .nodes
        .len() as u64
}

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
    .build(crate::testing::runtime_lease_owner())?;
    let id = SessionId::fixture("history-hydration");
    core.session(id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
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
