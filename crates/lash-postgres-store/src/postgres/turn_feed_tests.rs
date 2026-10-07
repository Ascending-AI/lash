//! The turn-change feed's clock against concurrent commits (FIG-5275), each
//! on its own isolated database.

// Test code: the server comes from the environment the target's runner
// hands it.
#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use lash_core_execution::{SessionCatalogStore as _, SessionCommitStore as _, SessionId};

use crate::testing::{AfterReceipt, IsolatedDatabase};

async fn isolated() -> Option<IsolatedDatabase> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping the turn-feed clock law: database URL is not set");
        return None;
    };
    Some(IsolatedDatabase::create(&database_url).await)
}

fn persisted_state(session_id: &SessionId) -> lash_core_execution::RuntimeSessionState {
    lash_core_execution::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    }
}

/// A commit held after it recorded its turn receipt does not hold another
/// session's commit: the shared clock is taken by each commit's last
/// statement, so the other commit takes the next sequence and lands while
/// the first is still held, and the feed keeps commit order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_turn_commit_does_not_hold_another_sessions_commit() {
    let Some(database) = isolated().await else {
        return;
    };
    let seam = AfterReceipt::new();
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store")
        .with_after_receipt_for_testing(seam.clone());
    let store = storage.store();
    let warm = SessionId::from("warm-turn-commit");
    let held = SessionId::from("held-turn-commit");
    let free = SessionId::from("free-turn-commit");
    for session_id in [&warm, &held, &free] {
        store
            .admit_session(
                &lash_core_execution::testing::store_fixtures::root_session_request(session_id),
            )
            .await
            .expect("admit the session");
    }
    // The two racing commits store the same content-addressed blobs; a first
    // commit of the same state lands them, so neither waits on the other's
    // uncommitted copy.
    store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&persisted_state(&warm)),
        )
        .await
        .expect("the warm-up commit lands");

    let pause = seam.pause_next();
    let holder = tokio::spawn({
        let store = store.clone();
        let state = persisted_state(&held);
        async move {
            store
                .commit_runtime_state(
                    lash_core_execution::RuntimeCommit::persisted_state_for_test(&state),
                )
                .await
        }
    });
    pause.reached().await;
    let state = persisted_state(&free);
    tokio::time::timeout(
        Duration::from_secs(5),
        store.commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state),
        ),
    )
    .await
    .expect("the other session's commit waited behind the held one")
    .expect("the other session's commit lands while the first is held");
    assert!(!holder.is_finished(), "the held commit is still held");

    pause.release();
    holder
        .await
        .expect("join the held commit")
        .expect("the held commit lands once released");
    let order: Vec<String> = sqlx::query_scalar(
        "SELECT session_id FROM lash_runtime_turn_commits WHERE session_id <> $1
         ORDER BY change_seq",
    )
    .bind(warm.as_str())
    .fetch_all(storage.pool())
    .await
    .expect("read the receipts in feed order");
    assert_eq!(
        order,
        [free.as_str(), held.as_str()],
        "the commit that landed first holds the lower sequence"
    );
}
