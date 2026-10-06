//! The open-time migration's laws, executed by the synthetic successor
//! (ADR 0115 §6): this build writes the database one version past its
//! schema-version constant, and a store stamped at the constant without the
//! successor's objects is the release before it.
#![expect(
    clippy::expect_used,
    reason = "test module: clippy's allow-expect-in-tests only exempts #[test] functions, and the fixture helpers here are test code too"
)]
#![expect(
    clippy::disallowed_methods,
    reason = "migration fixtures read their owned temporary stores and backups and contend for the store ownership lock"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_execution::{SessionCatalogStore as _, SessionId, StoreError};
use rusqlite::{Connection, OpenFlags};

use super::{
    SqliteBackupLocation, SqliteMigrationBackup, SqliteMigrationFault, SqliteMigrationHook,
    SqliteMigrationStep,
};
use crate::{SqliteStoreSet, SqliteStoreSetOptions};

use SqliteMigrationStep::{
    BackupSealed, BackupStarted, Committed, Completed, Copied, Locked, Migrated, Owned,
};

const FILE: &str = "lash.db";

/// The rows the predecessor fixture writes, one per family of tables.
const FIXTURE_ROWS: [(&str, &str); 3] = [
    (
        "session_meta",
        "INSERT INTO session_meta (session_id, relation_kind) VALUES ('fixture-session', 'root')",
    ),
    (
        "process_tombstones",
        "INSERT INTO process_tombstones \
         (process_id, terminal_label, pruned_at_ms, pruned_change_seq) \
         VALUES ('fixture', 'completed', 7, 7)",
    ),
    (
        "trigger_mutation_receipts",
        "INSERT INTO trigger_mutation_receipts \
         (operation_id, owner_kind, owner_id, request_fingerprint, result_json, created_at_ms) \
         VALUES ('fixture', 'host', 'h', 'f', '{}', 7)",
    ),
];

fn database_path(root: &Path) -> PathBuf {
    crate::location::canonical_path(root).join(FILE)
}

fn raw(root: &Path) -> Connection {
    let connection =
        Connection::open_with_flags(database_path(root), OpenFlags::SQLITE_OPEN_READ_WRITE)
            .expect("open a raw connection");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy timeout");
    connection
}

/// Wait until no connection holds the database: the last close removes the
/// write-ahead log, leaving the file the whole of its database.
fn quiesce(root: &Path) {
    {
        let connection = raw(root);
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("checkpoint");
    }
    let log = super::sidecar(&database_path(root), "-wal");
    let deadline = Instant::now() + Duration::from_secs(20);
    while log.exists() {
        assert!(
            Instant::now() < deadline,
            "a connection to the database never closed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn bytes(root: &Path) -> Vec<u8> {
    quiesce(root);
    std::fs::read(database_path(root)).expect("read database")
}

fn options(hook: Option<SqliteMigrationHook>) -> SqliteStoreSetOptions {
    SqliteStoreSetOptions {
        migration_hook: hook,
        ..SqliteStoreSetOptions::default()
    }
}

async fn open(root: &Path, options: SqliteStoreSetOptions) -> Result<SqliteStoreSet, StoreError> {
    SqliteStoreSet::open_with_options_and_clock(
        root.join(FILE),
        options,
        Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await
    .map_err(crate::sqlite_async_error)
}

/// Turn the migrated store at `root` back into the shape the release before
/// this build wrote: the successor's objects absent, the stamp one behind.
fn rewind(root: &Path) {
    raw(root)
        .execute_batch(
            "DROP INDEX idx_lash_synthetic_next_note;
             DROP TABLE lash_synthetic_next;
             UPDATE lash_compat SET version = version - 1 WHERE singleton = 1;",
        )
        .expect("write the predecessor's shape");
}

/// A store the release before this build wrote: provisioned, one row of each
/// family, the successor's objects absent and the stamp at the predecessor's
/// version. Answers the root and the database's bytes.
async fn predecessor() -> (tempfile::TempDir, Vec<u8>) {
    let root = tempfile::tempdir().expect("store root");
    drop(
        open(root.path(), options(None))
            .await
            .expect("provision the store"),
    );
    for (_, insert) in FIXTURE_ROWS {
        raw(root.path())
            .execute_batch(insert)
            .expect("write a predecessor row");
    }
    rewind(root.path());
    let before = bytes(root.path());
    assert_eq!(stamp(root.path()), 0);
    (root, before)
}

/// The predecessor's version and the one this build writes.
fn versions() -> (i64, i64) {
    let descriptor = lash_core_execution::compat::descriptor(crate::schema::COMPONENT)
        .expect("the build declares the database");
    (
        i64::from(descriptor.reads.min()),
        i64::from(descriptor.writes.max()),
    )
}

/// How far the stamp is past the predecessor's version: 0 before the
/// migration, 1 after it.
fn stamp(root: &Path) -> i64 {
    let version: i64 = raw(root)
        .query_row("SELECT version FROM lash_compat", [], |row| row.get(0))
        .expect("read the stamp");
    version - versions().0
}

/// The store is at this build's version, kept every fixture row, carries the
/// successor's objects, and its components read and write it.
async fn assert_migrated(root: &Path) {
    assert_eq!(stamp(root), 1, "the database is migrated");
    let connection = raw(root);
    for (table, _) in FIXTURE_ROWS {
        let rows: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count fixture rows");
        assert_eq!(rows, 1, "{table} kept its row");
    }
    let successor: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name IN \
             ('lash_synthetic_next', 'idx_lash_synthetic_next_note')",
            [],
            |row| row.get(0),
        )
        .expect("read the catalog");
    assert_eq!(successor, 2, "the database gained the successor's objects");
    drop(connection);
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
        "the store serves its row after the migration"
    );
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                lash_core_execution::SessionMeta {
                    owning_process_id: None,
                    session_id: SessionId::from("after-migration"),
                    relation: lash_core_execution::SessionRelation::Root,
                    pending_observer_intents: Vec::new(),
                },
            ),
        )
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

/// The backup at `directory` holds the database, byte for byte as `before`.
fn assert_backup_holds(directory: &Path, manifest: &serde_json::Value, before: &[u8]) {
    let copy = std::fs::read(directory.join(FILE)).expect("read the copy");
    assert!(
        copy == before,
        "the backup is the database as it was before the migration"
    );
    assert_eq!(manifest["file"], FILE);
    assert_eq!(manifest["from"], versions().0);
    assert_eq!(manifest["to"], versions().1);
    assert_eq!(manifest["bytes"], before.len());
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

const FRESH_STEPS: [SqliteMigrationStep; 8] = [
    Owned,
    BackupStarted,
    Copied,
    BackupSealed,
    Locked,
    Migrated,
    Committed,
    Completed,
];

/// Opening a store older than the build copies the database, whole and
/// synced, before the migration's lock or first write; the copy is the store
/// as it was, and the migrated store serves its rows.
#[tokio::test]
async fn sqlite_open_backs_up_the_database_before_migrating() {
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
            *at_seal.lock().expect("copy") =
                std::fs::read(directory.join(FILE)).expect("read the copy");
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
        FRESH_STEPS,
        "the database is copied before it is locked"
    );
    assert!(
        *backup_at_seal.lock().expect("copy") == before,
        "when the backup seals, it holds the database as it was"
    );
    let backups = backups(root.path());
    assert_eq!(backups.len(), 1, "one migration, one backup");
    let (directory, manifest) = &backups[0];
    assert_eq!(manifest["state"], "migrated");
    assert_backup_holds(directory, manifest, &before);
    assert_migrated(root.path()).await;
}

/// A crash at any step of a migration leaves a store the next open
/// completes: a migration whose transaction committed is recorded finished,
/// and one that did not commit starts again from a fresh backup.
#[tokio::test]
async fn sqlite_migration_crash_at_every_step_resumes() {
    for crash_at in FRESH_STEPS {
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
}

/// A migration that fails before its transaction commits changes nothing:
/// the database is byte for byte what it was, its backup goes with the
/// error, and the next open migrates.
#[tokio::test]
async fn sqlite_migration_failure_before_commit_changes_nothing() {
    for fail_at in [Owned, BackupStarted, Copied, BackupSealed, Locked, Migrated] {
        let (root, before) = predecessor().await;
        let (hook, steps) = recording(move |step| {
            if step == fail_at {
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
            failed.to_string().contains("injected"),
            "{fail_at:?}: {failed}"
        );
        assert!(
            !steps.lock().expect("steps").contains(&Committed),
            "{fail_at:?}: nothing committed"
        );
        assert!(
            bytes(root.path()) == before,
            "{fail_at:?}: the database is as it was"
        );
        assert!(
            backups(root.path()).is_empty(),
            "{fail_at:?}: the failed migration's backup goes with it"
        );
        drop(
            open(root.path(), options(None))
                .await
                .expect("the next open migrates"),
        );
        assert_migrated(root.path()).await;
    }
}

/// The migration owns the store: it takes the store's migrator lock,
/// checkpoints and closes the database, and refuses a store another
/// connection or another migrator holds, having changed nothing; then it
/// holds the database exclusively from its lock to its commit.
#[tokio::test]
async fn sqlite_migration_owns_the_store_and_holds_it_exclusively() {
    let (root, before) = predecessor().await;

    // Another connection on the database: the migration waits for it, then
    // refuses, and neither the store nor the backup location changed.
    let holder = raw(root.path());
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
        .open(crate::store_ownership::lock_path(&database_path(
            root.path(),
        )))
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
    let writable = move || {
        let probe = raw(&probe_root);
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
    let (hook, steps) = recording(move |step| {
        if matches!(step, Locked | Migrated | Committed) {
            assert_eq!(
                writable(),
                step == Committed,
                "at {step:?}, the database is held exactly when the migration holds it"
            );
        }
        SqliteMigrationFault::Proceed
    });
    drop(
        open(root.path(), options(Some(hook)))
            .await
            .expect("the open migrates the store"),
    );
    assert_eq!(steps.lock().expect("steps").clone(), FRESH_STEPS);
    assert_migrated(root.path()).await;
}

/// The backup location and retention are the host's: backups go only where
/// it says, and only as many finished backups of the store are kept as it
/// asks for.
#[tokio::test]
async fn sqlite_migration_backups_follow_the_configured_location_and_retention() {
    let (root, _) = predecessor().await;
    let elsewhere = tempfile::tempdir().expect("backup location");
    let configured = || SqliteStoreSetOptions {
        migration_backup: SqliteMigrationBackup {
            location: SqliteBackupLocation::Directory(elsewhere.path().to_path_buf()),
            retain: std::num::NonZeroUsize::MIN,
        },
        ..options(None)
    };
    drop(
        open(root.path(), configured())
            .await
            .expect("the first migration succeeds"),
    );
    assert_eq!(backups_in(elsewhere.path()).len(), 1);
    rewind(root.path());
    let rewound = bytes(root.path());
    drop(
        open(root.path(), configured())
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
    assert_backup_holds(&kept[0].0, &kept[0].1, &rewound);
    assert_migrated(root.path()).await;
}

/// A component opened on its own never migrates: its installer refuses a
/// database older than the build writes, typed, and leaves it unchanged.
#[tokio::test]
async fn sqlite_component_open_refuses_an_unmigrated_database() {
    let (root, before) = predecessor().await;
    let error = crate::SqliteTriggerStore::open(&database_path(root.path()))
        .await
        .map(drop)
        .map_err(crate::sqlite_async_error)
        .expect_err("an unmigrated database is refused");
    assert!(
        matches!(
            error,
            StoreError::Incompatible {
                refusal: lash_core_execution::compat::CompatRefusal::MigrationPending {
                    found,
                    target,
                    ..
                }
            } if (i64::from(found), i64::from(target)) == versions()
        ),
        "{error}"
    );
    assert!(
        bytes(root.path()) == before,
        "the refused open changed nothing"
    );
}
