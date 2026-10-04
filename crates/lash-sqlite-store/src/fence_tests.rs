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
    FleetFormat, FleetFormatStore, ProcessOriginator, SessionCatalogStore as _, SessionId,
    SessionMeta, SessionRelation, StoreError, StoreSet, TriggerCommand, TriggerOwnerScope,
    TriggerStore as _,
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

fn run_intent_exists(location: &SqliteLocation) -> bool {
    match location {
        SqliteLocation::File { root } => root.join("lash-finalize.json").exists(),
        SqliteLocation::Memory { .. } => false,
    }
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
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    raw(&location, SqliteDatabase::Triggers)
        .execute(
            "INSERT INTO trigger_mutation_receipts \
             (operation_id, owner_kind, owner_id, request_fingerprint, result_json, created_at_ms) \
             VALUES ('seeded', 'host', 'h', 'f', '{}', 0)",
            [],
        )
        .expect("seed a receipt so the fenced trigger mutation's insert is detectable");
    let core = set.process_env_store();
    core.admit_session(
        &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
            session_meta("before-finalize"),
        ),
    )
    .await
    .expect("a writer before finalize commits");
    let sessions_before = count(&location, SqliteDatabase::DurableCore, "session_meta");
    let mut writers = Vec::new();
    for database in SqliteDatabase::ALL {
        writers.push((database, writer(&location, database).await));
    }

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
        .execute_command(
            "fence-trigger-mutation",
            TriggerCommand::Prune {
                owner_scope: TriggerOwnerScope::host("fence").expect("host owner scope"),
                actor: ProcessOriginator::host(),
                subscription_keys: Vec::new(),
            },
        )
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
        "a fenced trigger writer journaled no receipt"
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
    let (_run, set) = file_set().await;
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
    let (_run, set) = file_set().await;
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
    let pauses = crate::testing::SqlitePauses::default();
    let set = SqliteStoreSet::open_with_options_and_clock(
        root.path(),
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
        count(&location, SqliteDatabase::DurableCore, "session_meta"),
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
    let path = root.path().join(crate::DURABLE_CORE_DB_FILE);
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
    assert_eq!(
        count(set.location(), SqliteDatabase::DurableCore, "session_meta"),
        1
    );
}

#[tokio::test]
async fn sqlite_fence_encodes_again_when_f_moves() {
    use lash_core_execution::{SessionCatalogStore as _, SessionCommitStore as _};
    let (_run, set) = file_set().await;
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
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    let receipt = store
        .commit_runtime_state(lash_core_execution::RuntimeCommit::persisted_state_for_test(&state))
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
    async fn unfinished_invocations(
        &self,
        _generation: &BuildGeneration,
    ) -> Result<u64, DeploymentRegistryError> {
        Ok(0)
    }

    async fn deployments_serving(
        &self,
        _generation: &BuildGeneration,
    ) -> Result<Vec<RetainedDeployment>, DeploymentRegistryError> {
        Ok(self.0.lock().expect("deployments").clone())
    }
}

/// Exit the finalizing process after either partial-set commit, then recover
/// with a fresh public open and no surviving SQLite handles.
#[cfg(feature = "synthetic-next")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[expect(
    clippy::disallowed_methods,
    reason = "the crash law starts its own test executable and exits without dropping database handles"
)]
async fn sqlite_finalize_cold_reopen_completes_partial_epoch_flip() {
    const TEST: &str = "fence_tests::sqlite_finalize_cold_reopen_completes_partial_epoch_flip";
    const RUN: &str = "LASH_SQLITE_FINALIZE_CRASH_ROOT";
    const CUT: &str = "LASH_SQLITE_FINALIZE_CRASH_CUT";
    if let Some(root) = std::env::var_os(RUN) {
        let cut = std::env::var(CUT).expect("child crash cut");
        let options = crate::SqliteStoreSetOptions {
            finalize_hook: Some(crate::testing::SqliteFinalizeHook::new(move |database| {
                if database.file_name() == cut {
                    std::process::exit(77);
                }
            })),
            ..crate::SqliteStoreSetOptions::default()
        };
        let set = SqliteStoreSet::open_with_options_and_clock(
            std::path::PathBuf::from(root),
            options,
            std::sync::Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
        .expect("the successor opens before finalize");
        set.finalize(
            &BuildGeneration::for_test("cold-finalize-old"),
            &Deployments::default(),
            &[],
            5,
        )
        .await
        .expect("finalize reaches the crash cut");
        panic!("the child did not crash");
    }

    let mut failures = Vec::new();
    for cut in [SqliteDatabase::DurableCore, SqliteDatabase::ProcessRegistry] {
        let (root, set) = file_set().await;
        let location = set.location().clone();
        set.generation_drain()
            .mark_draining(&BuildGeneration::for_test("cold-finalize-old"), 1)
            .await
            .expect("drain the retired generation");
        for database in SqliteDatabase::ALL {
            raw(&location, database)
                .execute(
                    "INSERT INTO lash_synthetic_next (id, note) VALUES (7, 'keep-me')",
                    [],
                )
                .expect("seed application rows");
            let (connection, _, sql) = writer(&location, database).await;
            let sql = if database == SqliteDatabase::ProcessRegistry {
                "INSERT INTO draining_generations (generation, marked_at_ms) VALUES ('0123456789ab', 7)"
            } else {
                sql
            };
            insert(&connection, sql)
                .await
                .expect("seed production application rows");
        }
        let application_rows: Vec<_> = SqliteDatabase::ALL
            .into_iter()
            .map(|database| {
                let table = match database {
                    SqliteDatabase::DurableCore => "session_meta",
                    SqliteDatabase::ProcessRegistry => "draining_generations",
                    SqliteDatabase::Triggers => "trigger_mutation_receipts",
                };
                let sql = format!("SELECT * FROM {table} ORDER BY 1");
                let rows = crate::testing::read_rows_for_testing(&set, database, &sql)
                    .expect("snapshot production application rows");
                (database, sql, rows)
            })
            .collect();
        drop(set);
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(RUN, root.path())
            .env(CUT, cut.file_name())
            .output()
            .expect("run the finalizing process");
        assert_eq!(
            output.status.code(),
            Some(77),
            "{cut:?}: exit at the committed cut\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        for database in SqliteDatabase::ALL {
            let connection = raw(&location, database);
            let (schema, fleet): (i64, i64) = connection
                .query_row("SELECT version, fleet_format FROM lash_compat", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .expect("read the partial set");
            assert_eq!(schema, database.expected_version());
            assert_eq!(
                fleet,
                if database <= cut { 2 } else { 1 },
                "{cut:?}: {database:?}"
            );
        }
        let reopened = match SqliteStoreSet::open(root.path()).await {
            Ok(set) => set,
            Err(error) => {
                failures.push(format!("after {cut:?}: {error}"));
                continue;
            }
        };
        for (database, sql, before) in application_rows {
            assert_eq!(
                crate::testing::read_rows_for_testing(&reopened, database, &sql)
                    .expect("read recovered application rows"),
                before,
                "{cut:?}: {database:?} application rows stay unchanged"
            );
        }
        for database in SqliteDatabase::ALL {
            let mut connection = raw(reopened.location(), database);
            let fleet: i64 = connection
                .query_row("SELECT fleet_format FROM lash_compat", [], |row| row.get(0))
                .expect("read recovered epoch");
            assert_eq!(fleet, 2, "{cut:?}: {database:?}");
            let note: String = connection
                .query_row(
                    "SELECT note FROM lash_synthetic_next WHERE id = 7",
                    [],
                    |row| row.get(0),
                )
                .expect("application row survived");
            assert_eq!(note, "keep-me");
            let tx = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .expect("a fresh N writer starts its transaction");
            let error = crate::sqlite_error(
                crate::compat::fence(&tx, database, VersionRange::new(1, 1).expect("N range"))
                    .expect_err("N cannot write the recovered epoch"),
            );
            assert!(
                matches!(error, StoreError::WriterFenced { recorded: 2, .. }),
                "{error}"
            );
        }
        drop(reopened);
        drop(
            SqliteStoreSet::open(root.path())
                .await
                .expect("recovery is idempotent"),
        );
    }
    assert!(
        failures.is_empty(),
        "cold recovery failed at both cuts: {failures:?}"
    );
}

#[cfg(feature = "synthetic-next")]
#[tokio::test]
async fn sqlite_cold_open_refuses_partial_epoch_without_finalize_intent() {
    let (root, set) = file_set().await;
    raw(set.location(), SqliteDatabase::DurableCore)
        .execute("UPDATE lash_compat SET fleet_format = 2", [])
        .expect("make an unauthorized mixed set");
    drop(set);
    let error = SqliteStoreSet::open(root.path())
        .await
        .map(drop)
        .expect_err("open refuses the mixed set");
    assert!(matches!(
        crate::sqlite_async_error(error),
        StoreError::Incompatible {
            refusal: CompatRefusal::PartiallyAdvanced { .. }
        }
    ));
}

#[cfg(feature = "synthetic-next")]
#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "the crash law invokes its child process and corrupts the recorded stamp and intent"
)]
async fn sqlite_finalize_cold_reopen_refuses_changed_stamp_or_intent() {
    let (root, set) = file_set().await;
    let location = set.location().clone();
    set.generation_drain()
        .mark_draining(&BuildGeneration::for_test("cold-finalize-old"), 1)
        .await
        .expect("drain the retiring generation");
    drop(set);
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "fence_tests::sqlite_finalize_cold_reopen_completes_partial_epoch_flip",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LASH_SQLITE_FINALIZE_CRASH_ROOT", root.path())
        .env(
            "LASH_SQLITE_FINALIZE_CRASH_CUT",
            SqliteDatabase::DurableCore.file_name(),
        )
        .output()
        .expect("crash the finalizing process");
    assert_eq!(
        output.status.code(),
        Some(77),
        "the finalizing child must exit at the committed cut\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let intent_path = root.path().join("lash-finalize.json");
    let original = std::fs::read(&intent_path).expect("the authorization survived the crash");
    for fault in ["stamp", "retirement", "target", "malformed"] {
        if fault == "stamp" {
            raw(&location, SqliteDatabase::Triggers)
                .execute("UPDATE lash_compat SET min_reader = version", [])
                .expect("change the last database's stamp");
        } else {
            let mut intent: serde_json::Value =
                serde_json::from_slice(&original).expect("intent JSON");
            let bytes = match fault {
                "retirement" => {
                    intent["retired"]["draining_since_ms"] = serde_json::Value::Null;
                    serde_json::to_vec(&intent).expect("encode an undrained authorization")
                }
                "target" => {
                    intent["target"] = serde_json::json!(3);
                    serde_json::to_vec(&intent).expect("encode an unsupported target")
                }
                _ => b"{".to_vec(),
            };
            std::fs::write(&intent_path, bytes).expect("corrupt the authorization");
        }
        let error = SqliteStoreSet::open(root.path())
            .await
            .map(drop)
            .expect_err("open refuses changed state");
        let error = crate::sqlite_async_error(error);
        assert!(
            matches!(
                error,
                StoreError::Incompatible {
                    refusal: CompatRefusal::MalformedStamp { .. }
                } | StoreError::WriterFenced { recorded: 3, .. }
            ),
            "{fault}: {error}"
        );
        for database in SqliteDatabase::ALL {
            let fleet: i64 = raw(&location, database)
                .query_row("SELECT fleet_format FROM lash_compat", [], |row| row.get(0))
                .expect("read the unchanged partial epoch");
            assert_eq!(
                fleet,
                if database == SqliteDatabase::DurableCore {
                    2
                } else {
                    1
                },
                "{fault}: {database:?}"
            );
        }
        raw(&location, SqliteDatabase::Triggers)
            .execute("UPDATE lash_compat SET min_reader = version - 1", [])
            .expect("restore the original stamp");
        std::fs::write(&intent_path, &original).expect("restore the original authorization");
    }
    drop(
        SqliteStoreSet::open(root.path())
            .await
            .expect("the original authorization still recovers"),
    );
    assert!(!intent_path.exists(), "completion clears the intent");
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
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let writable = FleetFormat::writable();
    let next = writable.max() + 1;
    let successor = VersionRange::new(writable.min(), next).expect("writable range");
    let retired = BuildGeneration::for_test("sqlite-finalize-old");
    let deployments = Deployments::default();
    let fleet = |database| {
        raw(&location, database)
            .query_row("SELECT fleet_format FROM lash_compat", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("read F")
    };

    match set
        .finalize_as(&retired, &deployments, &[], 5, successor)
        .await
    {
        Err(FinalizeError::Refused(FinalizeRefusal::GenerationNotDrained { .. })) => {}
        other => panic!("an undrained generation must refuse finalize: {other:?}"),
    }
    assert!(
        !run_intent_exists(&location),
        "an undrained generation authorizes nothing"
    );
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
    match set
        .finalize_as(&retired, &deployments, &[], 5, successor)
        .await
    {
        Err(FinalizeError::Refused(FinalizeRefusal::DeploymentsRetained { .. })) => {}
        other => panic!("a retained deployment must refuse finalize: {other:?}"),
    }
    assert!(
        !run_intent_exists(&location),
        "a retained deployment authorizes nothing"
    );
    for database in SqliteDatabase::ALL {
        assert_eq!(fleet(database), i64::from(writable.min()), "{database:?}");
    }
    let core = set.process_env_store();
    core.admit_session(
        &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
            session_meta("before-finalize"),
        ),
    )
    .await
    .expect("this build writes while finalize is refused");
    let mut writers = Vec::new();
    for database in SqliteDatabase::ALL {
        writers.push((database, writer(&location, database).await));
    }

    deployments.0.lock().expect("deployments").clear();
    let flip = set
        .finalize_as(&retired, &deployments, &[], 5, successor)
        .await
        .expect("finalize");
    assert_eq!(
        flip,
        FleetEpochFlip::Finalized {
            from: writable.min(),
            to: next
        }
    );
    for database in SqliteDatabase::ALL {
        assert_eq!(fleet(database), i64::from(next), "{database:?}");
    }

    let sessions = count(&location, SqliteDatabase::DurableCore, "session_meta");
    let error = core
        .admit_session(
            &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                session_meta("after-finalize"),
            ),
        )
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
        set.finalize_as(&retired, &deployments, &[], 5, successor)
            .await
            .expect("finalize reruns"),
        FleetEpochFlip::AlreadyFinalized { fleet: next }
    );
}
