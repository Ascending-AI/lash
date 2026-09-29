//! SQLite migrates on open, after a complete backup (ADR 0106 §5, ADR 0115
//! §2.2).
//!
//! A SQLite store is one host's three databases, and its open is the only
//! place it migrates: [`crate::SqliteStoreSet`] runs [`migrate_on_open`]
//! before any component opens. A component opened on its own never migrates;
//! its installer refuses a database older than the build writes as
//! `MigrationPending` ([`crate::compat::refuse_unmigrated`]).
//!
//! When any database's compatibility stamp is older than the version this
//! build writes, the open:
//!
//! 1. **owns** the store: it takes the store's migrator lock, a file lock
//!    that serializes migrating opens across processes, then checkpoints
//!    each database and closes it, and a
//!    write-ahead log that survives the close means another connection still
//!    holds the database, so the open waits for it, up to the busy timeout,
//!    and then refuses;
//! 2. **backs up** every database: the checkpointed files are copied byte for
//!    byte into a new directory under the configured
//!    [`SqliteMigrationBackup::location`], each copy synced, and the backup's
//!    manifest records `migrating` only once all three are durable;
//! 3. **migrates** under the store's lock order ([`advance_set_observed`]):
//!    `BEGIN EXCLUSIVE` on every database in [`SqliteDatabase::ALL`] order,
//!    a check that no database changed since its copy, each database's
//!    catalog steps and stamp, then the commits in the same order;
//! 4. records the backup `migrated` and keeps the newest
//!    [`SqliteMigrationBackup::retain`] finished backups of the store.
//!
//! An interrupted migration resumes. The manifest says what was under way:
//! an unfinished backup is discarded and taken again; a migration some
//! database already committed is completed forward from its stamps, since a
//! database whose stamp reached the target committed its whole step; a
//! migration nothing committed yet starts again from a fresh backup, because
//! the store may have been written since the old one. A migration that
//! *fails* after a database committed restores every database from the
//! backup, byte for byte, and the open reports the failure. A restore that is
//! itself interrupted is completed by the next open, whichever build runs it.

use std::io::{self, Write as _};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use lash_core_execution::compat::{self, CompatRefusal, CompatStamp};
use lash_core_execution::{Clock, StoreError};
use rusqlite::{Connection, OpenFlags, Transaction};
use serde::{Deserialize, Serialize};

use crate::SqliteDatabase;
use crate::compat::{AdvanceStep, advance_set_observed};
use crate::location::SqliteLocation;

/// One step of the migration catalog: the DDL that moves `database` from
/// compatibility version `from` to `to`. The step's stamp write is the
/// migrator's, in the same transaction.
pub(crate) struct SqliteMigration {
    pub(crate) database: SqliteDatabase,
    pub(crate) from: u32,
    pub(crate) to: u32,
    pub(crate) ddl: &'static str,
}

/// Phase A's synthetic successor (ADR 0115 §6) expands every database by a
/// table and a non-unique index, the SQLite twin of PostgreSQL's synthetic
/// expand. Neither constrains the writes of a build that does not know them,
/// so the build before it admits the migrated store as `Expanded`.
#[cfg(feature = "synthetic-next")]
const SYNTHETIC_NEXT_DDL: &str = "CREATE TABLE IF NOT EXISTS lash_synthetic_next (
    id INTEGER PRIMARY KEY,
    note TEXT
);
CREATE INDEX IF NOT EXISTS idx_lash_synthetic_next_note ON lash_synthetic_next(note);";

/// Every migration this build can run, in version order per database. 1.0
/// is the clean-slate release, so its catalog is empty; each later release
/// appends its steps, and a database it provisions runs them too
/// ([`provisioning_steps`]).
pub(crate) const CATALOG: &[SqliteMigration] = &[
    #[cfg(feature = "synthetic-next")]
    SqliteMigration {
        database: SqliteDatabase::DurableCore,
        from: 1,
        to: 2,
        ddl: SYNTHETIC_NEXT_DDL,
    },
    #[cfg(feature = "synthetic-next")]
    SqliteMigration {
        database: SqliteDatabase::ProcessRegistry,
        from: 1,
        to: 2,
        ddl: SYNTHETIC_NEXT_DDL,
    },
    #[cfg(feature = "synthetic-next")]
    SqliteMigration {
        database: SqliteDatabase::Triggers,
        from: 1,
        to: 2,
        ddl: SYNTHETIC_NEXT_DDL,
    },
];

/// The compatibility version this build writes for `database`.
pub(crate) fn target_version(database: SqliteDatabase) -> rusqlite::Result<u32> {
    compat::descriptor(database.component())
        .map(|descriptor| descriptor.writes.max())
        .ok_or_else(|| {
            crate::compat::malformed(database, "the build has no descriptor for this database")
        })
}

/// The catalog DDL a database this build creates runs after its schema and
/// fragments: every step up to the version it writes.
pub(crate) fn provisioning_steps(database: SqliteDatabase) -> impl Iterator<Item = &'static str> {
    let target = target_version(database).unwrap_or(1);
    CATALOG
        .iter()
        .filter(move |step| step.database == database && step.to <= target)
        .map(|step| step.ddl)
}

/// The catalog steps that move `database` from `from` to `to`, in order, or
/// `None` when the catalog has no such path.
fn catalog_path(
    database: SqliteDatabase,
    from: u32,
    to: u32,
) -> Option<Vec<&'static SqliteMigration>> {
    let mut steps = Vec::new();
    let mut at = from;
    while at < to {
        let step = CATALOG
            .iter()
            .find(|step| step.database == database && step.from == at && step.to <= to)?;
        steps.push(step);
        at = step.to;
    }
    Some(steps)
}

/// Where an open-time migration keeps the backup it takes before it changes
/// the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SqliteBackupLocation {
    /// `migration-backups/` under the store's root directory, beside its
    /// three databases.
    BesideStore,
    /// This directory, created if absent. Several stores may share it: each
    /// backup's manifest names the store it was taken from, and an open only
    /// ever resumes, restores or removes its own store's backups.
    Directory(PathBuf),
}

/// How a SQLite store's open-time migration backs the store up first.
///
/// A migration copies all three databases into one new directory under
/// `location` before it changes any of them, and keeps the newest `retain`
/// finished backups of the store. A backup an interrupted migration or
/// restore still needs is never removed. The default keeps two backups
/// beside the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteMigrationBackup {
    /// The directory backups are written under.
    pub location: SqliteBackupLocation,
    /// How many finished backups of this store to keep, newest first.
    pub retain: NonZeroUsize,
}

impl Default for SqliteMigrationBackup {
    fn default() -> Self {
        Self {
            location: SqliteBackupLocation::BesideStore,
            retain: NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN),
        }
    }
}

/// The directory [`SqliteBackupLocation::BesideStore`] names under a store's
/// root.
const BESIDE_STORE_DIRECTORY: &str = "migration-backups";
/// Every backup directory's name: this prefix and a six-digit sequence.
const BACKUP_PREFIX: &str = "sqlite-backup-";
const MANIFEST: &str = "manifest.json";
/// The file in a store's root whose advisory lock serializes its migrators.
const MIGRATOR_LOCK: &str = "lash-migration.lock";
const MANIFEST_STAGING: &str = "manifest.json.staging";

/// One observable point of an open-time migration.
///
/// A migration from scratch passes `Owned` for each database, `BackupStarted`,
/// `Copied` for each, `BackupSealed`, `Locked` for each, `Migrated` for each,
/// `Committed` for each and `Completed`. A restore passes `RestoreStarted`,
/// `Owned` and `Restored` for each database, and `RestoreCompleted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqliteMigrationStep {
    /// This database is checkpointed and closed, and no other connection
    /// holds it.
    Owned(SqliteDatabase),
    /// The backup directory exists and its manifest records `backing_up`.
    BackupStarted,
    /// This database's bytes are copied into the backup and synced.
    Copied(SqliteDatabase),
    /// The manifest records `migrating`: the backup is complete and durable.
    BackupSealed,
    /// `BEGIN EXCLUSIVE` holds this database.
    Locked(SqliteDatabase),
    /// This database's catalog steps and stamp are written, uncommitted.
    Migrated(SqliteDatabase),
    /// This database's migration committed.
    Committed(SqliteDatabase),
    /// The manifest records `migrated`.
    Completed,
    /// The manifest records `restoring`.
    RestoreStarted,
    /// This database's file is replaced by its backup copy.
    Restored(SqliteDatabase),
    /// The manifest records `restored`.
    RestoreCompleted,
}

/// What a [`SqliteMigrationHook`] makes the migration do at a step.
#[cfg(feature = "testing")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqliteMigrationFault {
    /// Carry on.
    Proceed,
    /// Stop dead, as a process killed here would: nothing is cleaned up, and
    /// uncommitted transactions roll back when their connections close.
    Crash,
    /// Fail here, as a failing statement or write would.
    Fail,
}

/// A test's view of an open-time migration: called at every
/// [`SqliteMigrationStep`], it can pause there (by blocking) or answer a
/// crash or a failure.
#[cfg(feature = "testing")]
#[derive(Clone)]
pub struct SqliteMigrationHook(
    std::sync::Arc<dyn Fn(SqliteMigrationStep) -> SqliteMigrationFault + Send + Sync>,
);

#[cfg(feature = "testing")]
impl SqliteMigrationHook {
    /// A hook that answers every step with `at`.
    pub fn new(
        at: impl Fn(SqliteMigrationStep) -> SqliteMigrationFault + Send + Sync + 'static,
    ) -> Self {
        Self(std::sync::Arc::new(at))
    }
}

#[cfg(feature = "testing")]
impl std::fmt::Debug for SqliteMigrationHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SqliteMigrationHook")
    }
}

/// Where the migration reports its steps. Production opens have no hook.
#[derive(Clone, Default)]
pub(crate) struct Probe {
    #[cfg(feature = "testing")]
    hook: Option<SqliteMigrationHook>,
}

impl Probe {
    #[cfg(feature = "testing")]
    pub(crate) fn hooked(hook: Option<SqliteMigrationHook>) -> Self {
        Self { hook }
    }

    fn at(&self, step: SqliteMigrationStep) -> Result<(), Stop> {
        #[cfg(feature = "testing")]
        if let Some(hook) = &self.hook {
            match (hook.0)(step) {
                SqliteMigrationFault::Proceed => {}
                SqliteMigrationFault::Crash => return Err(Stop::Crashed(step)),
                SqliteMigrationFault::Fail => {
                    return Err(Stop::Failed(storage(format!(
                        "the migration failed at {step:?} (injected)"
                    ))));
                }
            }
        }
        #[cfg(not(feature = "testing"))]
        let _ = step;
        Ok(())
    }
}

/// Why a migration stopped.
enum Stop {
    Failed(StoreError),
    /// A hook's simulated crash: the caller cleans nothing up.
    #[cfg(feature = "testing")]
    Crashed(SqliteMigrationStep),
}

impl From<rusqlite::Error> for Stop {
    fn from(error: rusqlite::Error) -> Self {
        Self::Failed(crate::sqlite_error(error))
    }
}

impl Stop {
    fn into_store_error(self) -> StoreError {
        match self {
            Self::Failed(error) => error,
            #[cfg(feature = "testing")]
            Self::Crashed(step) => storage(format!("the migration crashed at {step:?} (injected)")),
        }
    }
}

fn storage(message: String) -> StoreError {
    StoreError::StorageFailure {
        backend: crate::SQLITE_BACKEND,
        message,
    }
}

fn io_failure(what: impl std::fmt::Display, error: io::Error) -> Stop {
    Stop::Failed(storage(format!("{what}: {error}")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BackupState {
    /// Copies are being taken; the store is unchanged.
    BackingUp,
    /// The backup is complete; the store may be partly migrated.
    Migrating,
    /// The migration completed.
    Migrated,
    /// The store is being restored from the backup.
    Restoring,
    /// The store was restored from the backup.
    Restored,
}

impl BackupState {
    fn finished(self) -> bool {
        matches!(self, Self::Migrated | Self::Restored)
    }
}

/// A backup's `manifest.json`: which store it holds, what the migration it
/// belongs to was doing, and each database's copy.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    /// The store's identity ([`SqliteLocation::identity`]).
    store: String,
    state: BackupState,
    taken_at_ms: u64,
    databases: Vec<BackedUpDatabase>,
    /// Why the migration failed, once a restore began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackedUpDatabase {
    /// The database's file name, which its copy also carries.
    file: String,
    /// The stamp's version when the copy was taken.
    from: u32,
    /// The version the migration moves it to.
    to: u32,
    /// The copy's length.
    bytes: u64,
}

impl BackedUpDatabase {
    fn database(&self) -> Result<SqliteDatabase, Stop> {
        SqliteDatabase::ALL
            .into_iter()
            .find(|database| database.file_name() == self.file)
            .ok_or_else(|| {
                Stop::Failed(storage(format!(
                    "the backup manifest names an unknown database file {}",
                    self.file
                )))
            })
    }
}

/// One backup directory and its manifest.
struct Backup {
    directory: PathBuf,
    sequence: u64,
    manifest: Manifest,
}

impl Backup {
    fn entry(&self, database: SqliteDatabase) -> Result<&BackedUpDatabase, Stop> {
        self.manifest
            .databases
            .iter()
            .find(|entry| entry.file == database.file_name())
            .ok_or_else(|| {
                Stop::Failed(storage(format!(
                    "the backup at {} holds no copy of the {}",
                    self.directory.display(),
                    database.name()
                )))
            })
    }

    /// Record `state` durably: the manifest is written beside the old one,
    /// synced, renamed over it and the directory synced.
    fn record(&mut self, state: BackupState) -> Result<(), Stop> {
        self.manifest.state = state;
        let bytes = serde_json::to_vec_pretty(&self.manifest).map_err(|error| {
            Stop::Failed(storage(format!("encode the backup manifest: {error}")))
        })?;
        write_synced(&self.directory.join(MANIFEST_STAGING), &bytes)?;
        rename(
            &self.directory.join(MANIFEST_STAGING),
            &self.directory.join(MANIFEST),
        )?;
        sync_directory(&self.directory)
    }
}

/// Migrate the store at `location` when any database is older than this
/// build writes, or finish what an interrupted migration started. A memory
/// store is always this process's own build's, so it never migrates.
pub(crate) fn migrate_on_open(
    location: &SqliteLocation,
    backup: &SqliteMigrationBackup,
    busy_timeout: Duration,
    clock: &dyn Clock,
    probe: Probe,
) -> Result<(), StoreError> {
    let SqliteLocation::File { root } = location else {
        return Ok(());
    };
    let backups = match &backup.location {
        SqliteBackupLocation::BesideStore => root.join(BESIDE_STORE_DIRECTORY),
        SqliteBackupLocation::Directory(directory) => directory.clone(),
    };
    Migration {
        location,
        root,
        backups,
        retain: backup.retain.get(),
        busy_timeout,
        clock,
        probe,
    }
    .run()
    .map_err(Stop::into_store_error)
}

struct Migration<'a> {
    location: &'a SqliteLocation,
    root: &'a Path,
    backups: PathBuf,
    retain: usize,
    busy_timeout: Duration,
    clock: &'a dyn Clock,
    probe: Probe,
}

/// What an open found to do.
enum Plan {
    Nothing,
    /// Back the store up and migrate every database from its stamp.
    Fresh(Vec<BackedUpDatabase>),
}

impl Migration<'_> {
    fn live(&self, database: SqliteDatabase) -> PathBuf {
        self.root.join(database.file_name())
    }

    fn run(&self) -> Result<(), Stop> {
        // The migration needs the whole store; a store missing a database is
        // not one an older build finished opening, so its installers decide.
        if SqliteDatabase::ALL
            .into_iter()
            .any(|database| !self.live(database).exists())
        {
            return Ok(());
        }
        let identity = self.location.identity();
        if self.pending(&identity)?.is_none()
            && matches!(self.plan(&self.stamps()?)?, Plan::Nothing)
        {
            return Ok(());
        }
        let _migrator = self.exclusive()?;
        // Decided again under the lock: another process's migration may have
        // finished while this one waited for it.
        self.run_exclusive(identity)
    }

    /// Serialize the store's migrators across processes: an exclusive
    /// advisory lock on [`MIGRATOR_LOCK`] in the store root, held until the
    /// migration or restore ends. It is not a database file, so taking and
    /// releasing it leaves SQLite's own locks alone. Another migrator is
    /// waited for up to the busy timeout.
    fn exclusive(&self) -> Result<std::fs::File, Stop> {
        let path = self.root.join(MIGRATOR_LOCK);
        let file = open_lock_file(&path)?;
        let deadline = Instant::now() + self.busy_timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(Stop::Failed(storage(format!(
                        "another process is migrating the store at {}; open it again once \
                         that migration ends",
                        self.root.display()
                    ))));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(io_failure(format!("lock {}", path.display()), error));
                }
            }
        }
    }

    fn run_exclusive(&self, identity: String) -> Result<(), Stop> {
        let stamps = self.stamps()?;
        if let Some(pending) = self.pending(&identity)? {
            match pending.manifest.state {
                BackupState::Restoring => {
                    let failure = pending.manifest.failure.clone().unwrap_or_else(|| {
                        "an earlier open's migration failed and its restore was interrupted"
                            .to_owned()
                    });
                    return self.restore(pending, failure, &identity);
                }
                BackupState::BackingUp => remove_directory(&pending.directory)?,
                BackupState::Migrating if self.owns(&pending)? => {
                    if self.advanced(&pending, &stamps)? {
                        return self.resume(pending);
                    }
                    // Nothing committed, so the store is what it was, or
                    // newer if another build wrote it since: a fresh backup
                    // replaces this one.
                    remove_directory(&pending.directory)?;
                }
                // Another build's migration: this build cannot complete it,
                // and the set check refuses a partial set.
                BackupState::Migrating => return Ok(()),
                BackupState::Migrated | BackupState::Restored => {}
            }
        }
        match self.plan(&stamps)? {
            Plan::Nothing => Ok(()),
            Plan::Fresh(databases) => self.fresh(identity, databases),
        }
    }

    /// Each database's stamp, read-only.
    fn stamps(&self) -> Result<Vec<(SqliteDatabase, Option<CompatStamp>)>, Stop> {
        let mut stamps = Vec::with_capacity(SqliteDatabase::ALL.len());
        for database in SqliteDatabase::ALL {
            let connection = Connection::open_with_flags(
                self.location.target(database).uri(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )?;
            connection.busy_timeout(self.busy_timeout)?;
            let stamp = crate::compat::read(&connection, database)?.map(|(stamp, _)| stamp);
            stamps.push((database, stamp));
        }
        Ok(stamps)
    }

    /// Migrate when any database is older than this build writes and every
    /// one is stamped inside what this build reads and at most its version.
    /// Anything else is the installers' to admit or refuse.
    fn plan(&self, stamps: &[(SqliteDatabase, Option<CompatStamp>)]) -> Result<Plan, Stop> {
        let mut databases = Vec::with_capacity(stamps.len());
        let mut older = false;
        for (database, stamp) in stamps {
            let Some(stamp) = stamp else {
                return Ok(Plan::Nothing);
            };
            let reads = compat::descriptor(database.component())
                .map(|descriptor| descriptor.reads)
                .ok_or_else(|| {
                    crate::compat::malformed(
                        *database,
                        "the build has no descriptor for this database",
                    )
                })?;
            let target = target_version(*database)?;
            if stamp.version < reads.min() || stamp.version > target {
                return Ok(Plan::Nothing);
            }
            if catalog_path(*database, stamp.version, target).is_none() {
                return Err(Stop::Failed(storage(format!(
                    "this build's migration catalog has no path for the {} from version {} to {}",
                    database.name(),
                    stamp.version,
                    target
                ))));
            }
            older |= stamp.version < target;
            databases.push(BackedUpDatabase {
                file: database.file_name().to_owned(),
                from: stamp.version,
                to: target,
                bytes: 0,
            });
        }
        Ok(if older {
            Plan::Fresh(databases)
        } else {
            Plan::Nothing
        })
    }

    /// Whether a pending migration is this build's: it moves every database
    /// to the version this build writes.
    fn owns(&self, backup: &Backup) -> Result<bool, Stop> {
        for entry in &backup.manifest.databases {
            if entry.to != target_version(entry.database()?)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether any database of a pending migration has moved off the stamp
    /// its copy was taken at.
    fn advanced(
        &self,
        backup: &Backup,
        stamps: &[(SqliteDatabase, Option<CompatStamp>)],
    ) -> Result<bool, Stop> {
        for (database, stamp) in stamps {
            let from = backup.entry(*database)?.from;
            if stamp.map(|stamp| stamp.version) != Some(from) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn fresh(&self, identity: String, databases: Vec<BackedUpDatabase>) -> Result<(), Stop> {
        for database in SqliteDatabase::ALL {
            self.own(database)?;
            self.probe.at(SqliteMigrationStep::Owned(database))?;
        }
        let mut backup = self.start_backup(identity, databases)?;
        let sealed = (|| {
            self.probe.at(SqliteMigrationStep::BackupStarted)?;
            let copied = self.copy_all(&mut backup)?;
            backup.record(BackupState::Migrating)?;
            self.probe.at(SqliteMigrationStep::BackupSealed)?;
            Ok(copied)
        })();
        let copied = match sealed {
            Ok(copied) => copied,
            Err(Stop::Failed(error)) => {
                // Nothing changed the store: the backup goes with the error.
                remove_directory(&backup.directory)?;
                return Err(Stop::Failed(error));
            }
            #[cfg(feature = "testing")]
            Err(crashed @ Stop::Crashed(_)) => return Err(crashed),
        };
        self.advance(backup, Some(&copied))
    }

    fn resume(&self, backup: Backup) -> Result<(), Stop> {
        for database in SqliteDatabase::ALL {
            self.own(database)?;
            self.probe.at(SqliteMigrationStep::Owned(database))?;
        }
        self.advance(backup, None)
    }

    /// Copy every database into the backup, in [`SqliteDatabase::ALL`] order,
    /// answering each live file's length and modification time at its copy.
    fn copy_all(&self, backup: &mut Backup) -> Result<Vec<(u64, SystemTime)>, Stop> {
        let mut copied = Vec::with_capacity(SqliteDatabase::ALL.len());
        for database in SqliteDatabase::ALL {
            let live = self.live(database);
            let (bytes, modified) =
                copy_synced(&live, &backup.directory.join(database.file_name()))?;
            if let Some(entry) = backup
                .manifest
                .databases
                .iter_mut()
                .find(|entry| entry.file == database.file_name())
            {
                entry.bytes = bytes;
            }
            copied.push((bytes, modified));
            self.probe.at(SqliteMigrationStep::Copied(database))?;
        }
        sync_directory(&backup.directory)?;
        Ok(copied)
    }

    /// Lock the store in order, migrate each database from its stamp and
    /// commit in order. `copied` is the fresh path's record of each copy:
    /// a live file that changed since is refused before anything is written.
    fn advance(
        &self,
        mut backup: Backup,
        copied: Option<&[(u64, SystemTime)]>,
    ) -> Result<(), Stop> {
        let stopped: std::cell::RefCell<Option<Stop>> = std::cell::RefCell::new(None);
        let committed = std::cell::Cell::new(0_usize);
        // The advance speaks rusqlite errors; the migration's own stop rides
        // beside it and is what the caller sees.
        let stop = |stop: Stop| {
            *stopped.borrow_mut() = Some(stop);
            crate::sqlite_conversion_error(storage("the migration stopped".to_owned()))
        };
        let result = advance_set_observed(
            self.location,
            self.busy_timeout,
            |database, tx| {
                self.migrate_database(&backup, database, tx, copied)
                    .map_err(stop)
            },
            |step| {
                let step = match step {
                    AdvanceStep::Locked(database) => SqliteMigrationStep::Locked(database),
                    AdvanceStep::Committed(database) => {
                        committed.set(committed.get() + 1);
                        SqliteMigrationStep::Committed(database)
                    }
                };
                self.probe.at(step).map_err(stop)
            },
        );
        let failure = match result {
            Ok(()) => {
                backup.record(BackupState::Migrated)?;
                self.probe.at(SqliteMigrationStep::Completed)?;
                self.prune(&backup.manifest.store);
                return Ok(());
            }
            Err(error) => stopped.into_inner().unwrap_or_else(|| Stop::from(error)),
        };
        match failure {
            #[cfg(feature = "testing")]
            crashed @ Stop::Crashed(_) => Err(crashed),
            Stop::Failed(error) if committed.get() == 0 => {
                // Every transaction rolled back, so the store is as it was
                // backed up (or as another writer left it): no restore.
                remove_directory(&backup.directory)?;
                Err(Stop::Failed(error))
            }
            Stop::Failed(error) => {
                let identity = backup.manifest.store.clone();
                self.restore(backup, error.to_string(), &identity)
            }
        }
    }

    fn migrate_database(
        &self,
        backup: &Backup,
        database: SqliteDatabase,
        tx: &Transaction<'_>,
        copied: Option<&[(u64, SystemTime)]>,
    ) -> Result<(), Stop> {
        let entry = backup.entry(database)?;
        if let Some(copied) = copied {
            let index = SqliteDatabase::ALL
                .iter()
                .position(|candidate| *candidate == database)
                .unwrap_or_default();
            self.refuse_changed_since_copy(database, copied.get(index).copied())?;
        }
        let Some((stamp, _)) = crate::compat::read(tx, database)? else {
            return Err(Stop::Failed(StoreError::Incompatible {
                refusal: CompatRefusal::Unstamped {
                    component: database.component().as_str().to_owned(),
                    writing_release: crate::compat::writing_release(tx, database),
                },
            }));
        };
        if stamp.version == entry.to {
            // An earlier open committed this database's migration.
            return Ok(());
        }
        if stamp.version != entry.from {
            return Err(Stop::Failed(storage(format!(
                "the {} is at version {}, but its backup was taken at {}: another build \
                 changed it during the migration",
                database.name(),
                stamp.version,
                entry.from
            ))));
        }
        let steps = catalog_path(database, entry.from, entry.to).ok_or_else(|| {
            Stop::Failed(storage(format!(
                "this build's migration catalog has no path for the {} from version {} to {}",
                database.name(),
                entry.from,
                entry.to
            )))
        })?;
        for step in steps {
            tx.execute_batch(step.ddl)?;
        }
        let stamped = tx.execute(
            "UPDATE lash_compat SET version = ?1 WHERE singleton = 1 AND version = ?2",
            [i64::from(entry.to), i64::from(entry.from)],
        )?;
        if stamped != 1 {
            return Err(Stop::Failed(storage(format!(
                "the {}'s stamp moved during its migration",
                database.name()
            ))));
        }
        self.probe.at(SqliteMigrationStep::Migrated(database))
    }

    /// Under the migration's lock, the live file must be the one that was
    /// copied: the same length and modification time, and an empty
    /// write-ahead log. Anything else is another process writing the store
    /// since the copy, and the migration stops before it writes.
    fn refuse_changed_since_copy(
        &self,
        database: SqliteDatabase,
        copied: Option<(u64, SystemTime)>,
    ) -> Result<(), Stop> {
        let live = self.live(database);
        let now = file_stat(&live)?;
        let log = file_stat(&sidecar(&live, "-wal")).ok();
        if Some(now) != copied || log.is_some_and(|(bytes, _)| bytes != 0) {
            return Err(Stop::Failed(storage(format!(
                "the {} changed after it was backed up: another process wrote the store \
                 during its migration, which needs the store to itself",
                database.name()
            ))));
        }
        Ok(())
    }

    /// Checkpoint `database` and close it; succeed once the close removed the
    /// write-ahead log, which SQLite does only for the last connection. A
    /// log that stays means another connection holds the database: wait for
    /// it up to the busy timeout, then refuse.
    fn own(&self, database: SqliteDatabase) -> Result<(), Stop> {
        let live = self.live(database);
        let log = sidecar(&live, "-wal");
        let deadline = Instant::now() + self.busy_timeout;
        loop {
            {
                let connection = Connection::open_with_flags(
                    self.location.target(database).uri(),
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
                )?;
                connection.busy_timeout(self.busy_timeout)?;
                // Reading the catalog rolls back a hot journal first.
                connection.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| {
                    row.get::<_, i64>(0)
                })?;
                let mode: String =
                    connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                if mode.eq_ignore_ascii_case("wal") {
                    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                }
                connection.close().map_err(|(_, error)| Stop::from(error))?;
            }
            if !log.exists() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Stop::Failed(storage(format!(
                    "the {} is open elsewhere, and its migration needs the store to itself: \
                     close every other connection to {} and open again",
                    database.name(),
                    self.root.display()
                ))));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn start_backup(
        &self,
        store: String,
        databases: Vec<BackedUpDatabase>,
    ) -> Result<Backup, Stop> {
        create_directories(&self.backups)?;
        let sequence = self
            .backup_directories()?
            .into_iter()
            .map(|(sequence, _)| sequence)
            .max()
            .unwrap_or(0)
            + 1;
        let directory = self.backups.join(format!("{BACKUP_PREFIX}{sequence:06}"));
        create_directory(&directory)?;
        sync_directory(&self.backups)?;
        let mut backup = Backup {
            directory,
            sequence,
            manifest: Manifest {
                store,
                state: BackupState::BackingUp,
                taken_at_ms: self.clock.timestamp_ms(),
                databases,
                failure: None,
            },
        };
        backup.record(BackupState::BackingUp)?;
        Ok(backup)
    }

    /// Replace every database with its copy, in [`SqliteDatabase::ALL`]
    /// order, and report `failure`. Each replacement is a synced copy renamed
    /// over the live file, so a crash leaves each database either as it was or
    /// restored, and the next open finishes the rest.
    fn restore(&self, mut backup: Backup, failure: String, identity: &str) -> Result<(), Stop> {
        backup.manifest.failure = Some(failure.clone());
        backup.record(BackupState::Restoring)?;
        self.probe.at(SqliteMigrationStep::RestoreStarted)?;
        for database in SqliteDatabase::ALL {
            self.own(database)?;
            self.probe.at(SqliteMigrationStep::Owned(database))?;
        }
        for database in SqliteDatabase::ALL {
            let entry = backup.entry(database)?;
            let copy = backup.directory.join(&entry.file);
            let (bytes, _) = file_stat(&copy)?;
            if bytes != entry.bytes {
                return Err(Stop::Failed(storage(format!(
                    "the backup copy {} is {bytes} bytes, not the {} its manifest records; \
                     the store is left for an operator to restore",
                    copy.display(),
                    entry.bytes
                ))));
            }
            let live = self.live(database);
            let staging = sidecar(&live, ".restoring");
            copy_synced(&copy, &staging)?;
            rename(&staging, &live)?;
            for suffix in ["-wal", "-shm"] {
                let path = sidecar(&live, suffix);
                if path.exists() {
                    remove_file(&path)?;
                }
            }
            sync_directory(self.root)?;
            self.probe.at(SqliteMigrationStep::Restored(database))?;
        }
        backup.record(BackupState::Restored)?;
        self.probe.at(SqliteMigrationStep::RestoreCompleted)?;
        self.prune(identity);
        Err(Stop::Failed(storage(format!(
            "the SQLite migration failed ({failure}); every database was restored from the \
             backup at {}, byte for byte, so the store is as it was before the migration",
            backup.directory.display()
        ))))
    }

    /// Every backup directory under the backup location, with its sequence.
    fn backup_directories(&self) -> Result<Vec<(u64, PathBuf)>, Stop> {
        if !self.backups.exists() {
            return Ok(Vec::new());
        }
        let mut directories = Vec::new();
        for entry in read_directory(&self.backups)? {
            let Some(sequence) = entry
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(BACKUP_PREFIX))
                .and_then(|sequence| sequence.parse::<u64>().ok())
            else {
                continue;
            };
            directories.push((sequence, entry));
        }
        directories.sort();
        Ok(directories)
    }

    /// This store's backups whose manifests read, oldest first.
    fn store_backups(&self, identity: &str) -> Result<Vec<Backup>, Stop> {
        let mut backups = Vec::new();
        for (sequence, directory) in self.backup_directories()? {
            let Ok(bytes) = read_file(&directory.join(MANIFEST)) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
                continue;
            };
            if manifest.store == identity {
                backups.push(Backup {
                    directory,
                    sequence,
                    manifest,
                });
            }
        }
        Ok(backups)
    }

    /// The newest of this store's backups whose migration or restore has not
    /// finished.
    fn pending(&self, identity: &str) -> Result<Option<Backup>, Stop> {
        Ok(self
            .store_backups(identity)?
            .into_iter()
            .filter(|backup| !backup.manifest.state.finished())
            .max_by_key(|backup| backup.sequence))
    }

    /// Keep the newest `retain` finished backups of this store. Pruning is
    /// housekeeping: a failure is logged, never the open's.
    fn prune(&self, identity: &str) {
        let backups = match self.store_backups(identity) {
            Ok(backups) => backups,
            Err(error) => {
                tracing::warn!(error = %error.into_store_error(), "could not list SQLite migration backups");
                return;
            }
        };
        let mut finished: Vec<_> = backups
            .into_iter()
            .filter(|backup| backup.manifest.state.finished())
            .collect();
        finished.sort_by_key(|backup| std::cmp::Reverse(backup.sequence));
        for backup in finished.into_iter().skip(self.retain) {
            if let Err(error) = remove_directory(&backup.directory) {
                tracing::warn!(
                    error = %error.into_store_error(),
                    backup = %backup.directory.display(),
                    "could not remove an expired SQLite migration backup"
                );
            }
        }
    }
}

/// `path` with `suffix` appended to its file name, as SQLite names its
/// `-wal` and `-shm` files.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

// The filesystem work of a backup and a restore. The store's host supplies
// the root and the backup location; these touch only files under them.

#[expect(
    clippy::disallowed_methods,
    reason = "a migration copies the host's store files into the host-configured backup location"
)]
fn copy_synced(from: &Path, to: &Path) -> Result<(u64, SystemTime), Stop> {
    let copy = || -> io::Result<(u64, SystemTime)> {
        let mut source = std::fs::File::open(from)?;
        let stat = source.metadata()?;
        let mut target = std::fs::File::create(to)?;
        let bytes = io::copy(&mut source, &mut target)?;
        target.flush()?;
        target.sync_all()?;
        Ok((bytes, stat.modified()?))
    };
    copy().map_err(|error| {
        io_failure(
            format!("copy {} to {}", from.display(), to.display()),
            error,
        )
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration locks a file in the host's store root to serialize migrators"
)]
fn open_lock_file(path: &Path) -> Result<std::fs::File, Stop> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| io_failure(format!("open {}", path.display()), error))
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration writes its manifest into the host-configured backup location"
)]
fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), Stop> {
    let write = || -> io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    write().map_err(|error| io_failure(format!("write {}", path.display()), error))
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration reads its manifests from the host-configured backup location"
)]
fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration stats the host's store files it copied"
)]
fn file_stat(path: &Path) -> Result<(u64, SystemTime), Stop> {
    let stat = || -> io::Result<(u64, SystemTime)> {
        let metadata = std::fs::metadata(path)?;
        Ok((metadata.len(), metadata.modified()?))
    };
    stat().map_err(|error| io_failure(format!("stat {}", path.display()), error))
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration and a restore replace files under the host's store and backup locations"
)]
fn rename(from: &Path, to: &Path) -> Result<(), Stop> {
    std::fs::rename(from, to).map_err(|error| {
        io_failure(
            format!("rename {} to {}", from.display(), to.display()),
            error,
        )
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "a restore removes the write-ahead sidecars of the host's store files it replaced"
)]
fn remove_file(path: &Path) -> Result<(), Stop> {
    std::fs::remove_file(path)
        .map_err(|error| io_failure(format!("remove {}", path.display()), error))
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration removes backups under the host-configured backup location"
)]
fn remove_directory(path: &Path) -> Result<(), Stop> {
    std::fs::remove_dir_all(path)
        .map_err(|error| io_failure(format!("remove {}", path.display()), error))
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration creates the host-configured backup location"
)]
fn create_directories(path: &Path) -> Result<(), Stop> {
    std::fs::create_dir_all(path)
        .map_err(|error| io_failure(format!("create {}", path.display()), error))
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration creates one backup directory under the host-configured backup location"
)]
fn create_directory(path: &Path) -> Result<(), Stop> {
    std::fs::create_dir(path)
        .map_err(|error| io_failure(format!("create {}", path.display()), error))
}

#[expect(
    clippy::disallowed_methods,
    reason = "a migration lists the host-configured backup location"
)]
fn read_directory(path: &Path) -> Result<Vec<PathBuf>, Stop> {
    let list = || -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    };
    list().map_err(|error| io_failure(format!("list {}", path.display()), error))
}

/// Sync a directory so the entries created or renamed in it are durable.
#[expect(
    clippy::disallowed_methods,
    reason = "a migration syncs the host's store and backup directories after it renames in them"
)]
fn sync_directory(path: &Path) -> Result<(), Stop> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_failure(format!("sync {}", path.display()), error))
}

#[cfg(all(test, feature = "synthetic-next"))]
#[path = "migration_tests.rs"]
mod laws;

#[cfg(test)]
mod catalog_tests {
    use super::*;

    /// Every stamp this build reads below the version it writes has a
    /// catalog path to it, so an open never finds a store it reads but
    /// cannot migrate.
    #[test]
    fn the_catalog_covers_every_readable_version() {
        for database in SqliteDatabase::ALL {
            let descriptor =
                compat::descriptor(database.component()).expect("every database has a descriptor");
            let target = descriptor.writes.max();
            for from in descriptor.reads.min()..=target {
                assert!(
                    catalog_path(database, from, target).is_some(),
                    "the {} has no catalog path from {from} to {target}",
                    database.name()
                );
            }
        }
    }
}
