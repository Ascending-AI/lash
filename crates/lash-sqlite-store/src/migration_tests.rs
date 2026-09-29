//! The open-time migration's laws, driven by the synthetic successor
//! (ADR 0115 §6): this build writes every database at version 2, and a
//! store stamped 1 without the successor's objects is the release before it.
#![expect(
    clippy::expect_used,
    reason = "test module: clippy's allow-expect-in-tests only exempts #[test] functions, and the fixture helpers here are test code too"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_execution::{SessionId, StoreError};
use rusqlite::{Connection, OpenFlags};

use super::{
    SqliteBackupLocation, SqliteMigrationBackup, SqliteMigrationFault, SqliteMigrationHook,
    SqliteMigrationStep,
};
use crate::{SqliteDatabase, SqliteStoreSet, SqliteStoreSetOptions};

use SqliteMigrationStep::{
    BackupSealed, BackupStarted, Committed, Completed, Copied, Locked, Migrated, Owned,
    RestoreCompleted, RestoreStarted, Restored,
};

const ALL: [SqliteDatabase; 3] = SqliteDatabase::ALL;

/// The rows the predecessor fixture writes, one per database.
fn fixture_row(database: SqliteDatabase) -> (&'static str, &'static str) {
    match database {
        SqliteDatabase::DurableCore => (
            "session_meta",
            "INSERT INTO session_meta (session_id, relation_kind) VALUES ('fixture-session', 'root')",
        ),
        SqliteDatabase::ProcessRegistry => (
            "draining_generations",
            "INSERT INTO draining_generations (generation, marked_at_ms) VALUES ('fixture', 7)",
        ),
        SqliteDatabase::Triggers => (
            "trigger_mutation_receipts",
            "INSERT INTO trigger_mutation_receipts \
             (operation_id, owner_kind, owner_id, request_fingerprint, result_json, created_at_ms) \
             VALUES ('fixture', 'host', 'h', 'f', '{}', 7)",
        ),
    }
}

fn database_path(root: &Path, database: SqliteDatabase) -> PathBuf {
    crate::location::canonical_path(root).join(database.file_name())
}

fn raw(root: &Path, database: SqliteDatabase) -> Connection {
    let connection = Connection::open_with_flags(
        database_path(root, database),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .expect("open a raw connection");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy timeout");
    connection
}

/// Wait until no connection holds any database: the last close removes the
/// write-ahead log, leaving each file the whole of its database.
fn quiesce(root: &Path) {
    for database in ALL {
        {
            let connection = raw(root, database);
            connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("checkpoint");
        }
        let log = super::sidecar(&database_path(root, database), "-wal");
        let deadline = Instant::now() + Duration::from_secs(20);
        while log.exists() {
            assert!(
                Instant::now() < deadline,
                "a connection to the {} never closed",
                database.name()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn bytes(root: &Path) -> Vec<Vec<u8>> {
    quiesce(root);
    ALL.into_iter()
        .map(|database| std::fs::read(database_path(root, database)).expect("read database"))
        .collect()
}

fn options(hook: Option<SqliteMigrationHook>) -> SqliteStoreSetOptions {
    SqliteStoreSetOptions {
        migration_hook: hook,
        ..SqliteStoreSetOptions::default()
    }
}

async fn open(root: &Path, options: SqliteStoreSetOptions) -> Result<SqliteStoreSet, StoreError> {
    SqliteStoreSet::open_with_options_and_clock(
        root,
        options,
        Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await
    .map_err(crate::sqlite_async_error)
}

/// A store the release before this build wrote: provisioned, one row in each
/// database, the successor's objects absent and every stamp at 1. Answers
/// the root and each database's bytes.
async fn predecessor() -> (tempfile::TempDir, Vec<Vec<u8>>) {
    let root = tempfile::tempdir().expect("store root");
    drop(
        open(root.path(), options(None))
            .await
            .expect("provision the store"),
    );
    for database in ALL {
        raw(root.path(), database)
            .execute_batch(&format!(
                "{};
                 DROP INDEX idx_lash_synthetic_next_note;
                 DROP TABLE lash_synthetic_next;
                 UPDATE lash_compat SET version = 1 WHERE singleton = 1;",
                fixture_row(database).1
            ))
            .expect("write the predecessor's shape");
    }
    let before = bytes(root.path());
    assert_eq!(stamps(root.path()), vec![1, 1, 1]);
    (root, before)
}

fn stamps(root: &Path) -> Vec<i64> {
    ALL.into_iter()
        .map(|database| {
            raw(root, database)
                .query_row("SELECT version FROM lash_compat", [], |row| row.get(0))
                .expect("read the stamp")
        })
        .collect()
}

/// The store is at this build's version, kept every fixture row, carries the
/// successor's objects, and its components read and write it.
async fn assert_migrated(root: &Path) {
    assert_eq!(stamps(root), vec![2, 2, 2], "every database is migrated");
    for database in ALL {
        let connection = raw(root, database);
        let (table, _) = fixture_row(database);
        let rows: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count fixture rows");
        assert_eq!(rows, 1, "the {} kept its row", database.name());
        let successor: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name IN \
                 ('lash_synthetic_next', 'idx_lash_synthetic_next_note')",
                [],
                |row| row.get(0),
            )
            .expect("read the catalog");
        assert_eq!(
            successor,
            2,
            "the {} gained the successor's objects",
            database.name()
        );
    }
    let set = open(root, options(None))
        .await
        .expect("the migrated store opens");
    let store = set.session_store_factory();
    assert!(
        store
            .load_session_meta(&SessionId::from("fixture-session"))
            .await
            .expect("read the migrated catalog")
            .is_some(),
        "the durable core serves its row after the migration"
    );
    store
        .save_session_meta(lash_core_execution::SessionMeta {
            owning_process_id: None,
            session_id: SessionId::from("after-migration"),
            relation: lash_core_execution::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
        })
        .await
        .expect("the migrated store admits this build's writers");
}

/// Every backup directory under `directory`, with its manifest.
fn backups_in(directory: &Path) -> Vec<(PathBuf, serde_json::Value)> {
    if !directory.exists() {
        return Vec::new();
    }
    let mut backups: Vec<_> = std::fs::read_dir(directory)
        .expect("list backups")
        .map(|entry| entry.expect("backup entry").path())
        .map(|path| {
            let manifest = serde_json::from_slice(
                &std::fs::read(path.join("manifest.json")).expect("read the manifest"),
            )
            .expect("decode the manifest");
            (path, manifest)
        })
        .collect();
    backups.sort_by(|left, right| left.0.cmp(&right.0));
    backups
}

fn backups(root: &Path) -> Vec<(PathBuf, serde_json::Value)> {
    backups_in(&crate::location::canonical_path(root).join("migration-backups"))
}

/// The backup at `directory` holds every database, byte for byte as `before`.
fn assert_backup_holds(directory: &Path, manifest: &serde_json::Value, before: &[Vec<u8>]) {
    for (index, database) in ALL.into_iter().enumerate() {
        let copy = std::fs::read(directory.join(database.file_name())).expect("read the copy");
        assert!(
            copy == before[index],
            "the backup's {} is the database as it was before the migration",
            database.name()
        );
        let entry = manifest["databases"]
            .as_array()
            .expect("manifest databases")
            .iter()
            .find(|entry| entry["file"] == database.file_name())
            .expect("the manifest names every database");
        assert_eq!(entry["from"], 1);
        assert_eq!(entry["to"], 2);
        assert_eq!(entry["bytes"], before[index].len());
    }
}

/// A hook that records every step and answers `fault` for it.
fn recording(
    fault: impl Fn(SqliteMigrationStep) -> SqliteMigrationFault + Send + Sync + 'static,
) -> (SqliteMigrationHook, Arc<Mutex<Vec<SqliteMigrationStep>>>) {
    let steps = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&steps);
    let hook = SqliteMigrationHook::new(move |step| {
        seen.lock().expect("steps").push(step);
        fault(step)
    });
    (hook, steps)
}

fn fresh_steps() -> Vec<SqliteMigrationStep> {
    ALL.into_iter()
        .map(Owned)
        .chain([BackupStarted])
        .chain(ALL.into_iter().map(Copied))
        .chain([BackupSealed])
        .chain(ALL.into_iter().map(Locked))
        .chain(ALL.into_iter().map(Migrated))
        .chain(ALL.into_iter().map(Committed))
        .chain([Completed])
        .collect()
}

fn restore_steps() -> Vec<SqliteMigrationStep> {
    [RestoreStarted]
        .into_iter()
        .chain(ALL.into_iter().map(Owned))
        .chain(ALL.into_iter().map(Restored))
        .chain([RestoreCompleted])
        .collect()
}

/// Opening a store older than the build copies all three databases, whole
/// and synced, before the first lock or write of the migration; the copies
/// are the store as it was, and the migrated store serves its rows.
#[tokio::test]
async fn sqlite_open_backs_up_every_database_before_migrating() {
    let (root, before) = predecessor().await;
    let backup_at_seal = Arc::new(Mutex::new(Vec::new()));
    let at_seal = Arc::clone(&backup_at_seal);
    let backup_root = crate::location::canonical_path(root.path()).join("migration-backups");
    let (hook, steps) = recording(move |step| {
        if step == BackupSealed {
            let (directory, manifest) = backups_in(&backup_root)
                .pop()
                .expect("the sealed backup exists");
            assert_eq!(manifest["state"], "migrating");
            *at_seal.lock().expect("copies") = ALL
                .into_iter()
                .map(|database| {
                    std::fs::read(directory.join(database.file_name())).expect("read the copy")
                })
                .collect();
        }
        SqliteMigrationFault::Proceed
    });
    drop(
        open(root.path(), options(Some(hook)))
            .await
            .expect("the open migrates the store"),
    );
    let steps = steps.lock().expect("steps").clone();
    assert_eq!(
        steps,
        fresh_steps(),
        "every database is copied before any is locked"
    );
    assert!(
        *backup_at_seal.lock().expect("copies") == before,
        "when the backup seals, it holds every database as it was"
    );
    let backups = backups(root.path());
    assert_eq!(backups.len(), 1, "one migration, one backup");
    let (directory, manifest) = &backups[0];
    assert_eq!(manifest["state"], "migrated");
    assert_backup_holds(directory, manifest, &before);
    assert_migrated(root.path()).await;
}

/// A crash at any step of a migration leaves a store the next open
/// completes: it resumes a migration some database committed, and starts
/// one nothing committed again from a fresh backup. A crash at any step of
/// a restore is finished by the next open, which leaves the store byte for
/// byte as it was backed up; the open after that migrates it.
#[tokio::test]
async fn sqlite_migration_crash_at_every_step_resumes_or_restores() {
    for crash_at in fresh_steps() {
        let (root, before) = predecessor().await;
        let (hook, _) = recording(move |step| {
            if step == crash_at {
                SqliteMigrationFault::Crash
            } else {
                SqliteMigrationFault::Proceed
            }
        });
        let crashed = open(root.path(), options(Some(hook)))
            .await
            .map(drop)
            .expect_err("the migration crashed");
        assert!(
            crashed.to_string().contains("crashed"),
            "{crash_at:?}: {crashed}"
        );
        drop(
            open(root.path(), options(None))
                .await
                .unwrap_or_else(|error| panic!("the open after a crash at {crash_at:?}: {error}")),
        );
        let backups = backups(root.path());
        assert_eq!(
            backups.len(),
            1,
            "{crash_at:?}: one finished backup remains, and no unfinished one"
        );
        let (directory, manifest) = &backups[0];
        assert_eq!(manifest["state"], "migrated", "{crash_at:?}");
        assert_backup_holds(directory, manifest, &before);
        assert_migrated(root.path()).await;
    }

    for crash_at in restore_steps() {
        let (root, before) = predecessor().await;
        let restoring = Arc::new(Mutex::new(false));
        let (hook, _) = recording(move |step| {
            let mut restoring = restoring.lock().expect("restore flag");
            *restoring |= step == RestoreStarted;
            if step == Committed(SqliteDatabase::ProcessRegistry) {
                SqliteMigrationFault::Fail
            } else if *restoring && step == crash_at {
                SqliteMigrationFault::Crash
            } else {
                SqliteMigrationFault::Proceed
            }
        });
        let crashed = open(root.path(), options(Some(hook)))
            .await
            .map(drop)
            .expect_err("the restore crashed");
        assert!(
            crashed.to_string().contains("crashed"),
            "{crash_at:?}: {crashed}"
        );
        match open(root.path(), options(None)).await {
            Err(restored) => {
                assert!(
                    restored.to_string().contains("restored"),
                    "{crash_at:?}: the next open finishes the restore and reports it: {restored}"
                );
                assert!(
                    bytes(root.path()) == before,
                    "{crash_at:?}: the finished restore is byte for byte the store as backed up"
                );
                drop(
                    open(root.path(), options(None))
                        .await
                        .expect("the open after a restore migrates"),
                );
            }
            // The crash came after the restore was recorded finished, so
            // the next open migrated the restored store from a new backup.
            Ok(set) => {
                assert_eq!(crash_at, RestoreCompleted);
                drop(set);
            }
        }
        let backups = backups(root.path());
        assert_eq!(
            backups.len(),
            2,
            "{crash_at:?}: the restored and the migrated backup"
        );
        assert_eq!(backups[0].1["state"], "restored", "{crash_at:?}");
        assert_eq!(backups[1].1["state"], "migrated", "{crash_at:?}");
        for (directory, manifest) in &backups {
            assert_backup_holds(directory, manifest, &before);
        }
        assert_migrated(root.path()).await;
    }
}

/// A migration that fails after some databases committed restores every
/// database from the backup: each file is byte for byte what it was before
/// the open, with no write-ahead log beside it, and the open reports the
/// failure. The next open migrates.
#[tokio::test]
async fn sqlite_backup_restores_byte_identical_state() {
    let (root, before) = predecessor().await;
    let (hook, steps) = recording(|step| {
        if step == Committed(SqliteDatabase::ProcessRegistry) {
            SqliteMigrationFault::Fail
        } else {
            SqliteMigrationFault::Proceed
        }
    });
    let failed = open(root.path(), options(Some(hook)))
        .await
        .map(drop)
        .expect_err("the migration fails");
    assert!(
        failed.to_string().contains("restored from the backup"),
        "the open reports the restore: {failed}"
    );
    let steps = steps.lock().expect("steps").clone();
    assert!(
        steps.contains(&Committed(SqliteDatabase::DurableCore))
            && !steps.contains(&Committed(SqliteDatabase::Triggers)),
        "two databases committed before the failure, and the third did not"
    );
    assert_eq!(
        steps[steps.len() - restore_steps().len()..],
        restore_steps()[..],
        "the failure restored every database, in order"
    );
    for database in ALL {
        let path = database_path(root.path(), database);
        for sidecar in ["-wal", "-shm"] {
            assert!(
                !super::sidecar(&path, sidecar).exists(),
                "the restored {} has no {sidecar}",
                database.name()
            );
        }
    }
    let restored: Vec<Vec<u8>> = ALL
        .into_iter()
        .map(|database| std::fs::read(database_path(root.path(), database)).expect("read"))
        .collect();
    assert!(
        restored == before,
        "every restored database is byte for byte the store before the migration"
    );
    let backups = backups(root.path());
    assert_eq!(backups.len(), 1);
    let (directory, manifest) = &backups[0];
    assert_eq!(manifest["state"], "restored");
    assert!(
        manifest["failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("injected")),
        "the manifest records why the migration failed: {manifest}"
    );
    assert_backup_holds(directory, manifest, &before);

    drop(
        open(root.path(), options(None))
            .await
            .expect("the next open migrates"),
    );
    assert_migrated(root.path()).await;
}

/// The migration owns the whole store under its lock order: it takes the
/// store's migrator lock, checkpoints and closes each database in
/// [`SqliteDatabase::ALL`] order, and refuses a store another connection or
/// another migrator holds, having changed nothing; then it takes
/// `BEGIN EXCLUSIVE` on every database in that order before it writes any,
/// holds each one exactly from its lock to its commit, and commits in the
/// same order.
#[tokio::test]
async fn sqlite_migration_respects_lock_order_across_the_three_databases() {
    let (root, before) = predecessor().await;

    // Another connection on a later database: the migration waits for it,
    // then refuses, and neither the store nor the backup location changed.
    let holder = raw(root.path(), SqliteDatabase::ProcessRegistry);
    holder
        .query_row("SELECT COUNT(*) FROM lash_compat", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("the other connection reads");
    let mut busy = options(None);
    busy.store.connection_policy.busy_timeout = Duration::from_millis(300);
    let refused = open(root.path(), busy)
        .await
        .map(drop)
        .expect_err("a store held elsewhere is not migrated");
    assert!(
        refused.to_string().contains("open elsewhere"),
        "the refusal names the other connection: {refused}"
    );
    drop(holder);
    assert!(
        bytes(root.path()) == before,
        "the refused open changed nothing"
    );
    assert!(
        backups(root.path()).is_empty(),
        "the refused open took no backup"
    );

    // Another process migrating the store holds its migrator lock: this open
    // waits for it, then refuses, again having changed nothing.
    let migrator = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(crate::location::canonical_path(root.path()).join(super::MIGRATOR_LOCK))
        .expect("open the migrator lock");
    migrator.lock().expect("hold the migrator lock");
    let mut busy = options(None);
    busy.store.connection_policy.busy_timeout = Duration::from_millis(300);
    let refused = open(root.path(), busy)
        .await
        .map(drop)
        .expect_err("a store another migrator holds is not migrated");
    assert!(
        refused.to_string().contains("another process is migrating"),
        "the refusal names the other migrator: {refused}"
    );
    drop(migrator);
    assert!(
        bytes(root.path()) == before,
        "the refused open changed nothing"
    );
    assert!(
        backups(root.path()).is_empty(),
        "the refused open took no backup"
    );

    let probe_root = root.path().to_path_buf();
    let writable = move |database: SqliteDatabase| {
        let probe = raw(&probe_root, database);
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
    let (hook, steps) = recording(move |step| {
        let held: Option<Vec<SqliteDatabase>> = match step {
            Locked(locked) => Some(
                ALL.into_iter()
                    .filter(|database| *database <= locked)
                    .collect(),
            ),
            Migrated(_) => Some(ALL.to_vec()),
            Committed(committed) => Some(
                ALL.into_iter()
                    .filter(|database| *database > committed)
                    .collect(),
            ),
            _ => None,
        };
        if let Some(held) = held {
            for database in ALL {
                assert_eq!(
                    writable(database),
                    !held.contains(&database),
                    "at {step:?}, the {} is held exactly when the migration holds it",
                    database.name()
                );
            }
        }
        SqliteMigrationFault::Proceed
    });
    drop(
        open(root.path(), options(Some(hook)))
            .await
            .expect("the open migrates the store"),
    );
    assert_eq!(
        steps.lock().expect("steps").clone(),
        fresh_steps(),
        "ownership, copies, locks, migrations and commits all follow SqliteDatabase::ALL"
    );
    assert_migrated(root.path()).await;
}

/// The backup location and retention are the host's: backups go only where
/// it says, and only as many finished backups of the store are kept as it
/// asks for.
#[tokio::test]
async fn sqlite_migration_backups_follow_the_configured_location_and_retention() {
    let (root, before) = predecessor().await;
    let elsewhere = tempfile::tempdir().expect("backup location");
    let configured = |hook: Option<SqliteMigrationHook>| SqliteStoreSetOptions {
        migration_backup: SqliteMigrationBackup {
            location: SqliteBackupLocation::Directory(elsewhere.path().to_path_buf()),
            retain: std::num::NonZeroUsize::MIN,
        },
        ..options(hook)
    };
    let (fail, _) = recording(|step| {
        if step == Committed(SqliteDatabase::DurableCore) {
            SqliteMigrationFault::Fail
        } else {
            SqliteMigrationFault::Proceed
        }
    });
    open(root.path(), configured(Some(fail)))
        .await
        .map(drop)
        .expect_err("the first migration fails and restores");
    assert_eq!(backups_in(elsewhere.path()).len(), 1);
    drop(
        open(root.path(), configured(None))
            .await
            .expect("the second migration succeeds"),
    );
    assert!(
        backups(root.path()).is_empty(),
        "nothing is backed up beside the store"
    );
    let kept = backups_in(elsewhere.path());
    assert_eq!(
        kept.len(),
        1,
        "retain = 1 keeps only the newest finished backup"
    );
    assert_eq!(kept[0].1["state"], "migrated");
    assert_backup_holds(&kept[0].0, &kept[0].1, &before);
    assert_migrated(root.path()).await;
}

/// A component opened on its own never migrates: its installer refuses a
/// database older than the build writes, typed, and leaves it unchanged.
#[tokio::test]
async fn sqlite_component_open_refuses_an_unmigrated_database() {
    let (root, before) = predecessor().await;
    let error =
        crate::SqliteTriggerStore::open(&database_path(root.path(), SqliteDatabase::Triggers))
            .await
            .map(drop)
            .map_err(crate::sqlite_async_error)
            .expect_err("an unmigrated trigger database is refused");
    assert!(
        matches!(
            error,
            StoreError::Incompatible {
                refusal: lash_core_execution::compat::CompatRefusal::MigrationPending {
                    found: 1,
                    target: 2,
                    ..
                }
            }
        ),
        "{error}"
    );
    assert!(
        bytes(root.path()) == before,
        "the refused open changed nothing"
    );
}
