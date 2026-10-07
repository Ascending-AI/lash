//! The turn-change feed against concurrent commits (FIG-5275, FIG-5276),
//! each on its own isolated database.

// Test code: the server comes from the environment the target's runner
// hands it.
#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use lash_core_execution::{
    DeploymentStore as _, SessionCatalogStore as _, SessionCommitStore as _, SessionId,
};

use crate::testing::{BeforeTurnCommit, IsolatedDatabase};

/// Read the turn feed from its start, which sequences every committed change.
async fn read_the_feed(store: &crate::PostgresStore) {
    store
        .turns_changed_since(
            lash_core_execution::store::TurnChangeCursor::initial(),
            std::num::NonZeroUsize::MIN,
        )
        .await
        .expect("read the turn feed");
}

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

/// A commit held right before its `COMMIT`, its turn receipt written, does
/// not hold another session's commit: no writer takes the feed's clock, so
/// the other commit lands while the first is still held. A read in between
/// sequences the landed one, and the feed keeps commit order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_turn_commit_does_not_hold_another_sessions_commit() {
    let Some(database) = isolated().await else {
        return;
    };
    let seam = BeforeTurnCommit::new();
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store")
        .with_before_turn_commit_for_testing(seam.clone());
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
    read_the_feed(&store).await;

    pause.release();
    holder
        .await
        .expect("join the held commit")
        .expect("the held commit lands once released");
    read_the_feed(&store).await;
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

/// The feed's cursor law with a deliberately late committer: a fault held
/// right before its `COMMIT` while sixteen other sessions record theirs and a
/// reader polls. The reader sees each fault exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_polling_reader_never_skips_or_repeats_a_late_committed_change() {
    let Some(database) = isolated().await else {
        return;
    };
    let seam = BeforeTurnCommit::new();
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store")
        .with_before_turn_commit_for_testing(seam.clone());
    let arm: lash_core_execution::testing::turn_feed_law::ArmLateCommit = Box::new(move || {
        let pause = seam.pause_next();
        let reached = pause.clone();
        lash_core_execution::testing::turn_feed_law::LateCommit {
            reached: Box::pin(async move { reached.reached().await }),
            release: Box::new(move || pause.release()),
        }
    });
    lash_core_execution::testing::turn_feed_law::a_polling_reader_never_skips_or_repeats_a_turn_change(
        std::sync::Arc::new(storage.store()),
        16,
        Some(arm),
    )
    .await;
}
