//! The PostgreSQL writer fence against a finalize that moves `F` (ADR 0115
//! §2.2–2.4), each on its own isolated database.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use lash_core_execution::compat::VersionRange;
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::{
    FleetFormat, SessionCatalogStore as _, SessionCommitStore as _, SessionId, SessionMeta,
    SessionRelation, StoreError, WriterPin,
};

use super::WriterFence;
use crate::testing::{AfterFence, HeldFinalize, IsolatedDatabase, finalize_fleet_epoch};
use crate::{PostgresStorage, PostgresStoreConfig};

/// The epoch the synthetic next release finalizes to.
const NEXT: u32 = 2;

async fn isolated() -> Option<IsolatedDatabase> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping writer-fence proof: database URL is not set");
        return None;
    };
    Some(IsolatedDatabase::create(&database_url).await)
}

/// Stand `storage` in for a build whose writable range is `writable`,
/// opened under `opened`.
fn standing(
    mut storage: PostgresStorage,
    writable: VersionRange,
    opened: FleetFormat,
) -> PostgresStorage {
    storage.fence = WriterFence::new(writable, opened);
    storage
}

fn root_meta(session_id: &SessionId) -> SessionMeta {
    SessionMeta {
        owning_process_id: None,
        session_id: session_id.clone(),
        relation: SessionRelation::Root,
        pending_observer_intents: Vec::new(),
    }
}

async fn recorded_epoch(storage: &PostgresStorage) -> i32 {
    sqlx::query_scalar("SELECT format_version FROM lash_fleet_format WHERE singleton")
        .fetch_one(storage.pool())
        .await
        .expect("read the recorded epoch")
}

async fn meta_rows(storage: &PostgresStorage, session_id: &SessionId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM lash_session_meta WHERE session_id = $1")
        .bind(session_id.as_str())
        .fetch_one(storage.pool())
        .await
        .expect("count session meta rows")
}

/// Waits until some backend of this database waits on a lock.
async fn until_a_lock_waiter(storage: &PostgresStorage) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(storage.pool())
            .await
            .expect("read lock waiters");
            if waiting > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("finalize waits on the writer's share lock");
}

/// A writer that passed its fence holds `F` shared: finalize waits for it,
/// and the writer commits under the old epoch before `F` moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_fence_orders_a_writer_before_finalize() {
    let Some(database) = isolated().await else {
        return;
    };
    let seam = AfterFence::new();
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("open the isolated store")
        .with_after_fence_for_testing(seam.clone());
    let session_id = SessionId::from("fence-orders-writer");

    let mut pause = seam.pause_next();
    let writer = tokio::spawn({
        let store = storage.store();
        let meta = root_meta(&session_id);
        async move { store.save_session_meta(meta).await }
    });
    assert_eq!(
        pause.reached().await,
        1,
        "the writer's fence read the old epoch"
    );

    let finalize = tokio::spawn({
        let pool = storage.pool().clone();
        async move { finalize_fleet_epoch(&pool, NEXT).await }
    });
    until_a_lock_waiter(&storage).await;
    assert!(
        !finalize.is_finished(),
        "finalize must wait behind a writer holding the fence row"
    );
    assert_eq!(recorded_epoch(&storage).await, 1);

    pause.release();
    writer
        .await
        .expect("join the writer")
        .expect("the paused writer commits under the old epoch");
    finalize
        .await
        .expect("join finalize")
        .expect("finalize commits once the writer has");
    assert_eq!(meta_rows(&storage, &session_id).await, 1);
    assert_eq!(recorded_epoch(&storage).await, i32::try_from(NEXT).unwrap());
    assert_eq!(seam.passed(), vec![1]);
}

/// A writer that begins after finalize moved `F` past its build's range is
/// refused `WriterFenced` and writes nothing: through a pre-encoded session
/// write, and through a retried single-statement write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_fence_refuses_a_writer_after_finalize_with_zero_writes() {
    let Some(database) = isolated().await else {
        return;
    };
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("open the isolated store");
    finalize_fleet_epoch(storage.pool(), NEXT)
        .await
        .expect("finalize the next release");

    let session_id = SessionId::from("fence-refuses-writer");
    let error = storage
        .store()
        .save_session_meta(root_meta(&session_id))
        .await
        .expect_err("a writer after finalize is fenced");
    assert!(
        matches!(
            error,
            StoreError::WriterFenced { recorded: NEXT, writable }
                if writable == FleetFormat::writable()
        ),
        "expected WriterFenced at epoch {NEXT}, got {error:?}"
    );
    assert_eq!(meta_rows(&storage, &session_id).await, 0);

    let generation = BuildGeneration::from_digest([0xfe, 0xce, 0, 0, 0, 1]);
    let error = storage
        .generation_drain()
        .mark_draining(&generation, 1)
        .await
        .expect_err("a drain mark after finalize is fenced");
    assert!(
        matches!(error, StoreError::WriterFenced { recorded: NEXT, .. }),
        "expected WriterFenced, got {error:?}"
    );
    let marks: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_draining_generations")
        .fetch_one(storage.pool())
        .await
        .expect("count drain marks");
    assert_eq!(marks, 0, "a fenced writer wrote nothing");
    assert_eq!(
        storage.fleet_format().version(),
        1,
        "a refused epoch is not the one this build writes under"
    );
}

/// A commit encoded under the last observed `F` whose fence reads a moved,
/// still writable `F` rolls back, is encoded again under the new epoch, and
/// commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_fence_encodes_again_when_f_moves() {
    let Some(database) = isolated().await else {
        return;
    };
    // Epoch 1 pins the receipt at a version no build writes, so a receipt
    // encoded under it is told apart from one encoded under epoch 2.
    const PINNED: u32 = 7;
    let opened = FleetFormat::from_version(1).with_writer_pins(&[WriterPin {
        constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
        generation: 1,
        version: PINNED,
    }]);
    let storage = standing(
        PostgresStorage::connect(database.url())
            .await
            .expect("open the isolated store"),
        VersionRange::between(1, NEXT),
        opened,
    );
    let store = storage.store();
    let session_id = SessionId::from("fence-encodes-again");
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
        )
        .await
        .expect("admit the session under epoch 1");
    assert_eq!(store.fence.fleet(), opened);

    finalize_fleet_epoch(storage.pool(), NEXT)
        .await
        .expect("finalize the next release");
    let state = lash_core_execution::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    let receipt = store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
        .await
        .expect("the commit re-encodes under the moved epoch and lands");
    assert_eq!(
        receipt.schema_version,
        lash_core_execution::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION
    );

    let stored: String = sqlx::query_scalar(
        "SELECT result_json FROM lash_runtime_turn_commits WHERE session_id = $1",
    )
    .bind(session_id.as_str())
    .fetch_one(storage.pool())
    .await
    .expect("read the stored receipt");
    let stored: serde_json::Value = serde_json::from_str(&stored).expect("decode the receipt");
    assert_eq!(
        stored["schema_version"],
        serde_json::json!(lash_core_execution::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
        "the stored receipt was encoded under epoch {NEXT}, not epoch 1's pin {PINNED}"
    );
    assert_eq!(storage.fleet_format().version(), NEXT);
}

/// A fence that meets contention behind a finalize retries from a fresh
/// `BEGIN`, and the retry reads the epoch finalize committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_fence_retries_contended_with_a_fresh_read() {
    let Some(database) = isolated().await else {
        return;
    };
    let seam = AfterFence::new();
    let storage = standing(
        PostgresStorage::connect_with(
            database.url(),
            PostgresStoreConfig {
                lock_timeout: Some(Duration::from_millis(100)),
                ..PostgresStoreConfig::default()
            },
        )
        .await
        .expect("open the isolated store"),
        VersionRange::between(1, NEXT),
        FleetFormat::from_version(1),
    )
    .with_after_fence_for_testing(seam.clone());

    let held = HeldFinalize::begin(storage.pool(), NEXT)
        .await
        .expect("finalize holds the fence row");
    let generation = BuildGeneration::from_digest([0xfe, 0xce, 0, 0, 0, 2]);
    let writer = tokio::spawn({
        let drain = storage.generation_drain();
        let generation = generation.clone();
        async move { drain.mark_draining(&generation, 1).await }
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        while seam.contended() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the writer's fence times out behind finalize");
    held.commit().await.expect("finalize commits");

    assert!(
        writer
            .await
            .expect("join the writer")
            .expect("the retried writer commits under the new epoch"),
        "the drain mark is new"
    );
    assert!(seam.contended() >= 1);
    assert_eq!(
        seam.passed(),
        vec![NEXT],
        "the retry's fence read the epoch finalize committed"
    );
    assert_eq!(storage.fleet_format().version(), NEXT);
}
