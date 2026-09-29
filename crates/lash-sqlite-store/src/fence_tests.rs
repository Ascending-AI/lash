//! The SQLite writer fence (ADR 0115 §2.2–2.4): every write transaction reads
//! its database's `lash_compat` row first, a finalize fences writers in every
//! database, a stamp another process migrated is admitted again at the next
//! write, and a migration holds the whole store exclusively, in order.
#![expect(
    clippy::expect_used,
    reason = "test module: clippy's allow-expect-in-tests only exempts #[test] functions, and the fixture helpers here are test code too"
)]

use std::time::Duration;

use lash_core_execution::compat::{CompatRefusal, VersionRange};
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::fleet_finalize::{
    DeploymentRegistry, DeploymentRegistryError, FinalizeError, FinalizeRefusal, FleetEpochFlip,
    RetainedDeployment,
};
use lash_core_execution::{
    FLEET_FORMAT_VERSION, FleetFormat, FleetFormatStore, SessionId, SessionMeta, SessionRelation,
    StoreError, StoreSet, TriggerStore as _,
};
use rusqlite::Connection;

use crate::compat::AdvanceStep;
use crate::conn::SqliteConnection;
use crate::schema::ensure_versioned_schema;
use crate::{SqliteDatabase, SqliteLocation, SqliteStoreSet};

fn raw(location: &SqliteLocation, database: SqliteDatabase) -> Connection {
    let connection = Connection::open_with_flags(
        location.target(database).uri(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open a second connection, as another process would");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy timeout");
    connection
}

fn count(location: &SqliteLocation, database: SqliteDatabase, table: &str) -> i64 {
    raw(location, database)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn session_meta(id: &str) -> SessionMeta {
    SessionMeta {
        owning_process_id: None,
        session_id: SessionId::from(id),
        relation: SessionRelation::Root,
        pending_observer_intents: Vec::new(),
    }
}

async fn file_set() -> (tempfile::TempDir, SqliteStoreSet) {
    let root = tempfile::tempdir().expect("store root");
    let set = SqliteStoreSet::open(root.path())
        .await
        .expect("open the store set");
    (root, set)
}

/// A connection-level writer on `database`, admitted by its installer, and a
/// table it can insert one row into.
async fn writer(
    location: &SqliteLocation,
    database: SqliteDatabase,
) -> (SqliteConnection, &'static str, &'static str) {
    let connection = SqliteConnection::open(&location.target(database))
        .await
        .expect("open a writer connection");
    ensure_versioned_schema(&connection, database)
        .await
        .expect("the installer admits the database");
    let (table, insert) = match database {
        SqliteDatabase::DurableCore => (
            "session_meta",
            "INSERT INTO session_meta (session_id, relation_kind) \
             VALUES ('fence-probe-' || (SELECT COUNT(*) FROM session_meta), 'root')",
        ),
        SqliteDatabase::ProcessRegistry => (
            "draining_generations",
            "INSERT INTO draining_generations (generation, marked_at_ms) \
             VALUES ('fence-probe-' || (SELECT COUNT(*) FROM draining_generations), 0)",
        ),
        SqliteDatabase::Triggers => (
            "trigger_mutation_receipts",
            "INSERT INTO trigger_mutation_receipts \
             (operation_id, owner_kind, owner_id, request_fingerprint, result_json, created_at_ms) \
             VALUES ('fence-probe-' || (SELECT COUNT(*) FROM trigger_mutation_receipts), \
                     'host', 'h', 'f', '{}', 0)",
        ),
    };
    (connection, table, insert)
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

/// After a finalize moves `F` past this build's writable range, a writer in
/// each of the three databases, reached through the store's own ports, is
/// refused `WriterFenced` and writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_fence_refuses_a_writer_after_finalize_in_each_database() {
    let (_root, set) = file_set().await;
    let location = set.location().clone();
    raw(&location, SqliteDatabase::Triggers)
        .execute(
            "INSERT INTO trigger_mutation_receipts \
             (operation_id, owner_kind, owner_id, request_fingerprint, result_json, created_at_ms) \
             VALUES ('seeded', 'host', 'h', 'f', '{}', 0)",
            [],
        )
        .expect("seed a receipt the trigger writer would prune");
    let core = set.process_env_store();
    core.save_session_meta(session_meta("before-finalize"))
        .await
        .expect("a writer before finalize commits");
    let sessions_before = count(&location, SqliteDatabase::DurableCore, "session_meta");
    let mut writers = Vec::new();
    for database in SqliteDatabase::ALL {
        writers.push((database, writer(&location, database).await));
    }

    let next = FLEET_FORMAT_VERSION + 1;
    crate::compat::finalize(
        &location,
        Duration::from_secs(5),
        VersionRange::new(FLEET_FORMAT_VERSION, next).expect("writable range"),
    )
    .expect("finalize");

    let core_error = core
        .save_session_meta(session_meta("after-finalize"))
        .await
        .expect_err("the durable-core writer is fenced");
    assert!(is_fenced(&core_error, next), "{core_error}");
    assert_eq!(
        count(&location, SqliteDatabase::DurableCore, "session_meta"),
        sessions_before,
        "a fenced durable-core writer wrote nothing"
    );

    let registry_error = set
        .generation_drain()
        .mark_draining(&BuildGeneration::from_digest([7; 6]), 1)
        .await
        .expect_err("the process-registry writer is fenced");
    assert!(is_fenced(&registry_error, next), "{registry_error}");
    assert_eq!(
        count(
            &location,
            SqliteDatabase::ProcessRegistry,
            "draining_generations"
        ),
        0,
        "a fenced process-registry writer wrote nothing"
    );

    let trigger_error = set
        .trigger_store()
        .prune_mutation_receipts(u64::MAX)
        .await
        .expect_err("the trigger writer is fenced");
    assert!(
        trigger_error.to_string().contains("writer fenced"),
        "{trigger_error}"
    );
    assert_eq!(
        count(
            &location,
            SqliteDatabase::Triggers,
            "trigger_mutation_receipts"
        ),
        1,
        "a fenced trigger writer deleted nothing"
    );

    // The fence reads each database's own row: every connection-level
    // writer opened before finalize is refused typed, whichever database it
    // holds.
    for (database, (connection, table, sql)) in writers {
        let before = count(&location, database, table);
        let error = insert(&connection, sql)
            .await
            .expect_err("a writer after finalize is fenced");
        assert!(is_fenced(&error, next), "{database:?}: {error}");
        assert_eq!(count(&location, database, table), before, "{database:?}");
    }
}

/// Another process migrates a shared database while this one holds a
/// connection. The next write admits the stamp again inside its own
/// transaction: an expand is admitted and the write lands, and a contract
/// past this build's floor is refused typed with nothing written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_fence_readmits_a_stamp_migrated_by_another_process() {
    let (_root, set) = file_set().await;
    let location = set.location().clone();
    for database in SqliteDatabase::ALL {
        let (connection, table, sql) = writer(&location, database).await;
        insert(&connection, sql)
            .await
            .expect("a writer under the installed stamp commits");

        raw(&location, database)
            .execute("UPDATE lash_compat SET version = version + 1", [])
            .expect("another process expands the database");
        let before = count(&location, database, table);
        insert(&connection, sql)
            .await
            .expect("an expanded stamp under this build's floor is admitted");
        assert_eq!(
            count(&location, database, table),
            before + 1,
            "{database:?}"
        );

        raw(&location, database)
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
                } if component == database.component().as_str()
            ),
            "{database:?}: {error}"
        );
        assert_eq!(
            count(&location, database, table),
            before + 1,
            "{database:?}: a refused writer wrote nothing"
        );

        raw(&location, database)
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
            "{database:?}: {error}"
        );
    }
}

/// A migration takes `BEGIN EXCLUSIVE` on every database in
/// `SqliteDatabase::ALL` order and holds all three before it rewrites any,
/// then commits in the same order. At each step a writer is excluded from
/// exactly the databases the migration holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_migration_takes_every_database_exclusively_in_order() {
    let (_root, set) = file_set().await;
    let location = set.location().clone();
    let writable = |database: SqliteDatabase| {
        let probe = raw(&location, database);
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
            Err(error) => panic!("probe {database:?}: {error}"),
        }
    };
    let mut steps = Vec::new();
    let rewritten = std::cell::RefCell::new(Vec::new());
    crate::compat::advance_set_observed(
        &location,
        Duration::from_secs(5),
        |database, tx| {
            rewritten.borrow_mut().push(database);
            tx.execute("UPDATE lash_compat SET version = version + 1", [])
                .map(drop)
        },
        |step| {
            steps.push(step);
            let held: Vec<_> = SqliteDatabase::ALL
                .into_iter()
                .filter(|database| match step {
                    AdvanceStep::Locked(locked) => database <= &locked,
                    AdvanceStep::Committed(committed) => database > &committed,
                })
                .collect();
            for database in SqliteDatabase::ALL {
                assert_eq!(
                    writable(database),
                    !held.contains(&database),
                    "at {step:?}, {database:?} is held exactly when the migration holds it"
                );
            }
            if let AdvanceStep::Locked(SqliteDatabase::Triggers) = step {
                assert!(
                    rewritten.borrow().is_empty(),
                    "no database is rewritten before all three are held"
                );
            }
            Ok(())
        },
    )
    .expect("migrate the store");
    let order = SqliteDatabase::ALL;
    assert_eq!(
        steps,
        order
            .into_iter()
            .map(AdvanceStep::Locked)
            .chain(order.into_iter().map(AdvanceStep::Committed))
            .collect::<Vec<_>>(),
        "locks and commits both follow SqliteDatabase::ALL"
    );
    assert_eq!(
        rewritten.into_inner(),
        order.to_vec(),
        "rewrites follow SqliteDatabase::ALL"
    );

    // Every open writer admits the migrated stamp at its next fence, and a
    // reopen sees a consistent set.
    for database in SqliteDatabase::ALL {
        let (connection, _, sql) = writer(&location, database).await;
        insert(&connection, sql)
            .await
            .expect("the expanded database admits this build's writers");
    }
    crate::compat::check_set(&location).expect("the migrated set agrees");
}

/// A writer paused after its fence holds the database: finalize waits for
/// it, the paused writer commits under the old `F`, and the next writer is
/// fenced (ADR 0115 §2.2, the `AfterFence` seam).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_finalize_waits_for_a_writer_paused_after_its_fence() {
    let root = tempfile::tempdir().expect("store root");
    let injector = crate::testing::SqliteFaultInjector::default();
    let set = SqliteStoreSet::open_with_options_and_clock(
        root.path(),
        crate::SqliteStoreSetOptions {
            fault_injector: Some(injector.clone()),
            ..crate::SqliteStoreSetOptions::default()
        },
        std::sync::Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await
    .expect("open the store set");
    let location = set.location().clone();
    let core = set.process_env_store();

    let pause = injector.pause(crate::testing::SqliteFaultPoint::AfterFence);
    let paused = tokio::spawn({
        let core = std::sync::Arc::clone(&core);
        async move { core.save_session_meta(session_meta("straddles")).await }
    });
    pause.wait_until_reached().await;

    let next = FLEET_FORMAT_VERSION + 1;
    let finalize = tokio::task::spawn_blocking({
        let location = location.clone();
        move || {
            crate::compat::finalize(
                &location,
                Duration::from_secs(10),
                VersionRange::new(FLEET_FORMAT_VERSION, next).expect("writable range"),
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
        count(&location, SqliteDatabase::DurableCore, "session_meta"),
        1,
        "the straddling writer's row is kept"
    );
    let error = core
        .save_session_meta(session_meta("after"))
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
    let path = root.path().join(crate::DURABLE_CORE_DB_FILE);
    let next = FLEET_FORMAT_VERSION + 1;
    let store = crate::SqliteStore::open_with_fleet_writable_range_for_testing(
        &path,
        VersionRange::new(FLEET_FORMAT_VERSION, next).expect("writable range"),
    )
    .await
    .expect("open under a two-epoch writable range");
    assert_eq!(store.fleet_format().version(), FLEET_FORMAT_VERSION);
    Connection::open(&path)
        .expect("raw connection")
        .execute(
            "UPDATE lash_compat SET fleet_format = ?1",
            [i64::from(next)],
        )
        .expect("another build finalizes");
    store
        .save_session_meta(session_meta("under-next"))
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
    let (_root, set) = file_set().await;
    let factory = set.session_store_factory();
    let session = SessionId::from("fenced-delete");
    factory
        .save_session_meta(session_meta(session.as_str()))
        .await
        .expect("save session");
    crate::testing::finalize_fleet_format(set.location(), 2).expect("finalize");
    let error = factory
        .delete_session(&session)
        .await
        .expect_err("delete fenced");
    assert!(
        matches!(
            error.stop,
            lash_core_execution::MaintenanceStop::Failed(StoreError::WriterFenced {
                recorded: 2,
                ..
            })
        ),
        "{error:?}"
    );
    assert_eq!(
        count(set.location(), SqliteDatabase::DurableCore, "session_meta"),
        1
    );
}

#[tokio::test]
async fn sqlite_fence_encodes_again_when_f_moves() {
    use lash_core_execution::{SessionCatalogStore as _, SessionCommitStore as _};
    let (_root, set) = file_set().await;
    let path = set
        .location()
        .target(SqliteDatabase::DurableCore)
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
        ))
    };
    let receipt = store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
        .await
        .expect("commit re-encodes under writable epoch");
    assert_ne!(receipt.schema_version, 7);
    let stored: String = raw(set.location(), SqliteDatabase::DurableCore)
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

/// The engine's deployments, as the SQLite finalize law stands them up.
#[derive(Default)]
struct Deployments(std::sync::Mutex<Vec<RetainedDeployment>>);

#[async_trait::async_trait]
impl DeploymentRegistry for Deployments {
    async fn deployments_serving(
        &self,
        _generation: &BuildGeneration,
    ) -> Result<Vec<RetainedDeployment>, DeploymentRegistryError> {
        Ok(self.0.lock().expect("deployments").clone())
    }
}

/// The store set's finalize (FIG-3800 B): refused typed while the retired
/// generation is undrained or still has a deployment, with `F` unchanged in
/// every database and this build's writers still admitted. Once it moves
/// `F` past this build's range, a writer of this build in each of the three
/// databases — through the store's ports and through connections opened
/// before the finalize — is refused `WriterFenced` and writes nothing. A
/// rerun finds the set finalized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_writer_is_fenced_after_finalize() {
    let (_root, set) = file_set().await;
    let location = set.location().clone();
    let next = FLEET_FORMAT_VERSION + 1;
    let successor = VersionRange::new(FLEET_FORMAT_VERSION, next).expect("writable range");
    let retired = BuildGeneration::for_test("sqlite-finalize-old");
    let deployments = Deployments::default();
    let fleet = |database| {
        raw(&location, database)
            .query_row("SELECT fleet_format FROM lash_compat", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("read F")
    };

    match set.finalize_as(&retired, &deployments, 5, successor).await {
        Err(FinalizeError::Refused(FinalizeRefusal::GenerationNotDrained { .. })) => {}
        other => panic!("an undrained generation must refuse finalize: {other:?}"),
    }
    set.generation_drain()
        .mark_draining(&retired, 1)
        .await
        .expect("mark the retired generation draining");
    deployments
        .0
        .lock()
        .expect("deployments")
        .push(RetainedDeployment {
            id: "dp_old".to_owned(),
            uri: None,
        });
    match set.finalize_as(&retired, &deployments, 5, successor).await {
        Err(FinalizeError::Refused(FinalizeRefusal::DeploymentsRetained { .. })) => {}
        other => panic!("a retained deployment must refuse finalize: {other:?}"),
    }
    for database in SqliteDatabase::ALL {
        assert_eq!(
            fleet(database),
            i64::from(FLEET_FORMAT_VERSION),
            "{database:?}"
        );
    }
    let core = set.process_env_store();
    core.save_session_meta(session_meta("before-finalize"))
        .await
        .expect("this build writes while finalize is refused");
    let mut writers = Vec::new();
    for database in SqliteDatabase::ALL {
        writers.push((database, writer(&location, database).await));
    }

    deployments.0.lock().expect("deployments").clear();
    let flip = set
        .finalize_as(&retired, &deployments, 5, successor)
        .await
        .expect("finalize");
    assert_eq!(
        flip,
        FleetEpochFlip::Finalized {
            from: FLEET_FORMAT_VERSION,
            to: next
        }
    );
    for database in SqliteDatabase::ALL {
        assert_eq!(fleet(database), i64::from(next), "{database:?}");
    }

    let sessions = count(&location, SqliteDatabase::DurableCore, "session_meta");
    let error = core
        .save_session_meta(session_meta("after-finalize"))
        .await
        .expect_err("the durable-core writer is fenced");
    assert!(is_fenced(&error, next), "{error}");
    assert_eq!(
        count(&location, SqliteDatabase::DurableCore, "session_meta"),
        sessions
    );
    let error = set
        .generation_drain()
        .mark_draining(&BuildGeneration::for_test("sqlite-stale-mark"), 2)
        .await
        .expect_err("the process-registry writer is fenced");
    assert!(is_fenced(&error, next), "{error}");
    for (database, (connection, table, sql)) in writers {
        let before = count(&location, database, table);
        let error = insert(&connection, sql)
            .await
            .expect_err("a writer opened before finalize is fenced");
        assert!(is_fenced(&error, next), "{database:?}: {error}");
        assert_eq!(count(&location, database, table), before, "{database:?}");
    }

    assert_eq!(
        set.finalize_as(&retired, &deployments, 5, successor)
            .await
            .expect("finalize reruns"),
        FleetEpochFlip::AlreadyFinalized { fleet: next }
    );
}
