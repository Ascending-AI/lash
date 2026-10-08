//! The SQLite writer fence (ADR 0115 §2.2–2.4): every write transaction reads
//! the database's `lash_compat` row first, a finalize fences every writer, a
//! stamp another process migrated is admitted again at the next write, and a
//! migration holds the database exclusively.
#![expect(
    clippy::expect_used,
    reason = "test module: clippy's allow-expect-in-tests only exempts #[test] functions, and the fixture helpers here are test code too"
)]

use std::time::Duration;

use lash_core_execution::compat::{CompatRefusal, VersionRange};
use lash_core_execution::{
    FleetFormat, FleetFormatStore, ProcessOriginator, ProcessRegistrar as _,
    SessionCatalogStore as _, SessionId, SessionMeta, SessionRelation, StoreError,
};
use rusqlite::Connection;

use crate::compat::AdvanceStep;
use crate::conn::SqliteConnection;
use crate::schema::ensure_versioned_schema;
use crate::{SqliteLocation, SqliteStoreSet};

fn raw(location: &SqliteLocation) -> Connection {
    let connection = Connection::open_with_flags(
        location.target().uri(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open a second connection, as another process would");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy timeout");
    connection
}

fn count(location: &SqliteLocation, table: &str) -> i64 {
    raw(location)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn session_meta(id: &str) -> SessionMeta {
    SessionMeta {
        owning_process_id: None,
        session_id: SessionId::fixture(id),
        relation: SessionRelation::Root,
        pending_observer_intents: Vec::new(),
    }
}

async fn file_set() -> (tempfile::TempDir, SqliteStoreSet) {
    let root = tempfile::tempdir().expect("store root");
    let set = SqliteStoreSet::open(root.path().join("lash.db"))
        .await
        .expect("open the store set");
    (root, set)
}

/// A table of each family the database holds (the durable core, the process
/// registry), and an insert of one row into it.
const PROBES: [(&str, &str); 2] = [
    (
        "session_meta",
        "INSERT INTO session_meta (session_id, relation_kind) \
         VALUES ('fence-probe-' || (SELECT COUNT(*) FROM session_meta), 'root')",
    ),
    (
        "process_tombstones",
        "INSERT INTO process_tombstones \
         (process_id, terminal_label, pruned_at_ms, pruned_change_seq) \
         VALUES ('fence-probe-' || (SELECT COUNT(*) FROM process_tombstones), 'completed', 0, 0)",
    ),
];

/// A connection-level writer on the database, admitted by its installer.
async fn writer(location: &SqliteLocation) -> SqliteConnection {
    let connection = SqliteConnection::open(&location.target())
        .await
        .expect("open a writer connection");
    ensure_versioned_schema(&connection)
        .await
        .expect("the installer admits the database");
    connection
}

async fn insert(connection: &SqliteConnection, sql: &'static str) -> Result<(), StoreError> {
    connection
        .write(move |tx| tx.execute(sql, []).map(drop))
        .await
        .map_err(crate::sqlite_error)
}

fn is_fenced(error: &StoreError, recorded: u32) -> bool {
    matches!(
        error,
        StoreError::WriterFenced { recorded: found, writable }
            if *found == recorded && *writable == FleetFormat::writable()
    )
}

/// After a finalize moves `F` past this build's writable range, a writer of
/// each family's tables, reached through the store's own ports, is refused
/// `WriterFenced` and writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_fence_refuses_a_writer_of_each_family_after_finalize() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let core = set.process_env_store();
    core.admit_session(
        &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
            session_meta("before-finalize"),
        ),
    )
    .await
    .expect("a writer before finalize commits");
    let sessions_before = count(&location, "session_meta");
    let connection = writer(&location).await;

    let writable = FleetFormat::writable();
    let next = writable.max() + 1;
    crate::compat::flip_epoch_for_testing(
        &location,
        Duration::from_secs(5),
        VersionRange::new(writable.min(), next).expect("writable range"),
    )
    .expect("finalize");

    let core_error = core
        .admit_session(
            &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                session_meta("after-finalize"),
            ),
        )
        .await
        .expect_err("the session writer is fenced");
    assert!(is_fenced(&core_error, next), "{core_error}");
    assert_eq!(
        count(&location, "session_meta"),
        sessions_before,
        "a fenced session writer wrote nothing"
    );

    let processes_before = count(&location, "processes");
    let registry_error = set
        .process_registry()
        .register_process(lash_core_execution::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core_execution::ProcessProvenance::host(),
            lash_core_execution::Lifetime::Detached,
        ))
        .await
        .expect_err("the process-registry writer is fenced");
    assert!(
        registry_error.to_string().contains("writer fenced"),
        "{registry_error}"
    );
    assert_eq!(
        count(&location, "processes"),
        processes_before,
        "a fenced process-registry writer wrote nothing"
    );
    // A connection-level writer opened before finalize is refused typed,
    // whichever family's table it writes.
    for (table, sql) in PROBES {
        let before = count(&location, table);
        let error = insert(&connection, sql)
            .await
            .expect_err("a writer after finalize is fenced");
        assert!(is_fenced(&error, next), "{table}: {error}");
        assert_eq!(count(&location, table), before, "{table}");
    }
}

/// Another process migrates the database while this one holds a connection.
/// The next write admits the stamp again inside its own transaction: an
/// expand is admitted and the write lands, and a contract past this build's
/// floor is refused typed with nothing written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_fence_readmits_a_stamp_migrated_by_another_process() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let (table, sql) = PROBES[0];
    let connection = writer(&location).await;
    insert(&connection, sql)
        .await
        .expect("a writer under the installed stamp commits");

    raw(&location)
        .execute("UPDATE lash_compat SET version = version + 1", [])
        .expect("another process expands the database");
    let before = count(&location, table);
    insert(&connection, sql)
        .await
        .expect("an expanded stamp under this build's floor is admitted");
    assert_eq!(count(&location, table), before + 1);

    raw(&location)
        .execute("UPDATE lash_compat SET min_reader = version", [])
        .expect("another process contracts the database");
    let error = insert(&connection, sql)
        .await
        .expect_err("a floor above this build refuses the write");
    assert!(
        matches!(
            &error,
            StoreError::Incompatible {
                refusal: CompatRefusal::ReaderFloorAbove { component, .. }
            } if component == crate::schema::COMPONENT.as_str()
        ),
        "{error}"
    );
    assert_eq!(
        count(&location, table),
        before + 1,
        "a refused writer wrote nothing"
    );

    raw(&location)
        .execute("DELETE FROM lash_compat", [])
        .expect("another process deletes the stamp");
    let error = insert(&connection, sql)
        .await
        .expect_err("a missing stamp fails closed");
    assert!(
        matches!(
            &error,
            StoreError::Incompatible {
                refusal: CompatRefusal::Unstamped { .. }
            }
        ),
        "{error}"
    );
}

/// A migration takes `BEGIN EXCLUSIVE` on the database before it rewrites
/// it: a writer is excluded from the lock until the commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_migration_holds_the_database_exclusively() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let writable = || {
        let probe = raw(&location);
        probe
            .busy_timeout(Duration::ZERO)
            .expect("probe busy timeout");
        match probe.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {
                probe.execute_batch("ROLLBACK").expect("release the probe");
                true
            }
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                false
            }
            Err(error) => panic!("probe: {error}"),
        }
    };
    let mut steps = Vec::new();
    crate::compat::advance_observed(
        &location,
        Duration::from_secs(5),
        |tx| {
            assert!(!writable(), "the rewrite runs under the exclusive lock");
            tx.execute("UPDATE lash_compat SET version = version + 1", [])
                .map(drop)
        },
        |step| {
            steps.push(step);
            assert_eq!(
                writable(),
                step == AdvanceStep::Committed,
                "at {step:?}, the database is held exactly while the migration holds it"
            );
            Ok(())
        },
    )
    .expect("migrate the store");
    assert_eq!(steps, [AdvanceStep::Locked, AdvanceStep::Committed]);

    // An open writer admits the migrated stamp at its next fence.
    let connection = writer(&location).await;
    insert(&connection, PROBES[0].1)
        .await
        .expect("the expanded database admits this build's writers");
}

/// A writer paused after its fence holds the database: finalize waits for
/// it, the paused writer commits under the old `F`, and the next writer is
/// fenced (ADR 0115 §2.2, the `AfterFence` seam).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_finalize_waits_for_a_writer_paused_after_its_fence() {
    let root = tempfile::tempdir().expect("store root");
    let pauses = crate::testing::SqlitePauses::default();
    let set = SqliteStoreSet::open_with_options_and_clock(
        root.path().join("lash.db"),
        crate::SqliteStoreSetOptions {
            pauses: Some(pauses.clone()),
            ..crate::SqliteStoreSetOptions::default()
        },
        std::sync::Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await
    .expect("open the store set");
    let location = set.location().clone();
    let core = set.process_env_store();

    let pause = pauses.pause_after_fence();
    let paused = tokio::spawn({
        let core = std::sync::Arc::clone(&core);
        async move {
            core.admit_session(
                &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                    session_meta("straddles"),
                ),
            )
            .await
        }
    });
    pause.wait_until_reached().await;

    let writable = FleetFormat::writable();
    let next = writable.max() + 1;
    let finalize = tokio::task::spawn_blocking({
        let location = location.clone();
        move || {
            crate::compat::flip_epoch_for_testing(
                &location,
                Duration::from_secs(10),
                VersionRange::new(writable.min(), next).expect("writable range"),
            )
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !finalize.is_finished(),
        "finalize waits for the writer that passed its fence"
    );
    pause.release();
    paused
        .await
        .expect("paused writer task")
        .expect("the writer past its fence commits under the old F");
    finalize
        .await
        .expect("finalize task")
        .expect("finalize commits after the writer");
    assert_eq!(
        count(&location, "session_meta"),
        1,
        "the straddling writer's row is kept"
    );
    let error = core
        .admit_session(
            &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                session_meta("after"),
            ),
        )
        .await
        .expect_err("the next writer is fenced");
    assert!(is_fenced(&error, next), "{error}");
}

/// `F` moving inside this build's writable range is not a refusal: the write
/// runs under the new epoch, and the handle's `fleet_format()` answers the
/// last `F` its fence observed, not the open-time value (ADR 0115 §2.3).
#[tokio::test]
async fn sqlite_fence_observes_a_writable_move_of_f() {
    let root = tempfile::tempdir().expect("store root");
    let path = root.path().join("lash.db");
    let next = FleetFormat::writable().max() + 1;
    let writable = VersionRange::new(FleetFormat::writable().min(), next).expect("writable range");
    let store = crate::SqliteStore::open_with_fleet_writable_range_for_testing(&path, writable)
        .await
        .expect("open under a two-epoch writable range");
    // A fresh store is seeded at the epoch the widened range names.
    assert_eq!(
        store.fleet_format().version(),
        FleetFormat::seed(writable).version()
    );
    Connection::open(&path)
        .expect("raw connection")
        .execute(
            "UPDATE lash_compat SET fleet_format = ?1",
            [i64::from(next)],
        )
        .expect("another build finalizes");
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                session_meta("under-next"),
            ),
        )
        .await
        .expect("a writable move of F is admitted");
    assert_eq!(
        store.fleet_format().version(),
        next,
        "the handle answers the epoch its last fence read"
    );
}

#[tokio::test]
async fn sqlite_session_delete_after_finalize_stays_writer_fenced() {
    use lash_core_execution::SessionCatalogStore as _;
    let (_run, set) = file_set().await;
    let factory = set.session_store_factory();
    let session = SessionId::from("fenced-delete");
    factory
        .admit_session(
            &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                session_meta(session.as_str()),
            ),
        )
        .await
        .expect("save session");
    let next = FleetFormat::writable().max() + 1;
    crate::testing::finalize_fleet_format(set.location(), next).expect("finalize");
    let error = factory
        .delete_session(&session)
        .await
        .expect_err("delete fenced");
    assert!(
        matches!(
            error.stop,
            lash_core_execution::MaintenanceStop::Failed(StoreError::WriterFenced {
                recorded,
                ..
            }) if recorded == next
        ),
        "{error:?}"
    );
    assert_eq!(count(set.location(), "session_meta"), 1);
}

#[tokio::test]
async fn sqlite_fence_encodes_again_when_f_moves() {
    use lash_core_execution::{SessionCatalogStore as _, SessionCommitStore as _};
    let (_run, set) = file_set().await;
    let path = set
        .location()
        .target()
        .file_path()
        .expect("core file")
        .to_owned();
    let store = crate::SqliteStore::open_with_fleet_writable_range_for_testing(
        &path,
        VersionRange::between(1, 2),
    )
    .await
    .expect("open with next writer range")
    .with_fleet_format_for_testing(FleetFormat::from_version(1).with_writer_pins(&[
        lash_core_execution::WriterPin {
            constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
            generation: 1,
            version: 7,
        },
    ]));
    let session = SessionId::from("reencoded");
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session),
        )
        .await
        .expect("admit under old epoch");
    crate::testing::finalize_fleet_format(set.location(), 2).expect("finalize");
    let state = lash_core_execution::RuntimeSessionState {
        session_id: session.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    let receipt = store
        .commit_runtime_state(lash_core_execution::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("commit re-encodes under writable epoch");
    assert_ne!(receipt.schema_version, 7);
    let stored: String = raw(set.location())
        .query_row(
            "SELECT result_json FROM runtime_turn_commits WHERE session_id = ?1",
            [session.as_str()],
            |row| row.get(0),
        )
        .expect("stored receipt");
    let stored: serde_json::Value = serde_json::from_str(&stored).expect("decode receipt");
    assert_ne!(stored["schema_version"], serde_json::json!(7));
    assert_eq!(store.fleet_format().version(), 2);
}
