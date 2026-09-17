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
    let core = test_core(data_dir).await;
    let state = test_state(
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
    // No session was ever opened for either chat, so the session store
    // directory may only contain stores `fork_at` created before failing;
    // the compensator must have reclaimed every one of them.
    let session_stores = data_dir.join("lash-sessions");
    let leftovers: Vec<_> = if session_stores.exists() {
        std::fs::read_dir(&session_stores)
            .expect("session store dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_type()
                    .map(|file_type| file_type.is_dir())
                    .unwrap_or(false)
            })
            .map(|entry| entry.file_name())
            .collect()
    } else {
        Vec::new()
    };
    assert!(
        leftovers.is_empty(),
        "no orphaned session store may remain: {leftovers:?}"
    );
}
