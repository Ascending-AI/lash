//! FIG-3295 regression: a `fork_at` failure must abort through the same
//! compensator as a later failure, so a half-built fork leaves neither a
//! listed chat, nor a `fork_pending` marker, nor a session store the boot
//! sweep cannot see.

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;

use crate::db::AppDb;
use crate::routes::{ForkChatRequest, fork_chat};
use crate::state::test_support::{test_core, test_state};

#[tokio::test]
async fn a_failed_fork_leaves_no_pending_marker_or_orphaned_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let data_dir = temp.path();
    let double = crate::state::test_support::test_double().await;
    let core = test_core(&double).await;
    crate::state::test_support::serve_chat_discard(&double, &core).await;
    let state = test_state(
        &double,
        &core,
        AppDb::open(&data_dir.join("app.db")).expect("app db"),
    );
    let source = state
        .with_db(|db| db.create_chat("source", "mock-model", None))
        .await
        .expect("create source chat");
    // The product db carries a branch point for a node the session store
    // never retained, so `prepare_chat_fork` succeeds and `fork_at` fails
    // — the abort path under test.
    state
        .with_db({
            let chat_id = source.id.clone();
            move |db| {
                db.save_branch_point(&chat_id, "node-not-retained")
                    .map(|_| ())
            }
        })
        .await
        .expect("save branch point");

    let error = fork_chat(
        State(state.clone()),
        AxumPath(source.id.clone()),
        Json(
            serde_json::from_value::<ForkChatRequest>(
                serde_json::json!({ "node_id": "node-not-retained" }),
            )
            .expect("fork request decodes"),
        ),
    )
    .await
    .expect_err("fork at an unretained node fails");

    assert_eq!(error.status, StatusCode::CONFLICT);
    let chats = state
        .with_db(|db| db.list_chats())
        .await
        .expect("list chats");
    assert_eq!(
        chats
            .iter()
            .map(|chat| chat.id.as_str())
            .collect::<Vec<_>>(),
        [source.id.as_str()],
        "the aborted fork must not be listed"
    );
    let pending = state
        .with_db(|db| db.pending_chat_forks())
        .await
        .expect("pending forks");
    assert!(
        pending.is_empty(),
        "no fork_pending marker may outlive the abort"
    );
    // No session was ever opened for either chat, so the session catalog may
    // only hold stores `fork_at` created before failing; the compensator must
    // have reclaimed every one of them.
    let catalog = rusqlite::Connection::open(
        double
            .stores()
            .database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
    .expect("open the session catalog");
    let leftovers: i64 = catalog
        .query_row("SELECT count(*) FROM session_meta", [], |row| row.get(0))
        .expect("count catalogued sessions");
    assert_eq!(leftovers, 0, "no orphaned session store may remain");
}
