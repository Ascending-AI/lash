//! SQLite migrates on open, after a complete backup (ADR 0106 §5, ADR 0115
//! §2.2).
//!
//! A SQLite store is one database file, and its open is the only place it
//! migrates: [`crate::SqliteStoreSet`] runs [`migrate_on_open`] before any
//! component opens. A component opened on its own never migrates; its
//! installer refuses a database older than the build writes as
//! `MigrationPending` ([`crate::compat::refuse_unmigrated`]).
//!
//! When the database's compatibility stamp is older than the version this
//! build writes, the open:
//!
//! 1. **owns** the store: it takes the store's migrator lock, a file lock
//!    that serializes migrating opens across processes, then checkpoints the
//!    database and closes it, and a write-ahead log that survives the close
//!    means another connection still holds the database, so the open waits
//!    for it, up to the busy timeout, and then refuses;
//! 2. **backs up** the database: the checkpointed file is copied byte for
//!    byte into a new directory under the configured
//!    [`SqliteMigrationBackup::location`] and synced, and the backup's
//!    manifest records `migrating` only once the copy is durable;
//! 3. **migrates** in one `BEGIN EXCLUSIVE` transaction
//!    ([`advance_observed`]): a check that the database did not change since
//!    its copy, the catalog steps and the stamp, then the commit;
//! 4. records the backup `migrated` and keeps the newest
//!    [`SqliteMigrationBackup::retain`] finished backups of the store.
//!
//! An interrupted migration resumes. The manifest says what was under way:
//! an unfinished backup is discarded and taken again; a migration whose
//! transaction committed, which the stamp shows, is recorded finished; a
//! migration that did not commit starts again from a fresh backup, because
//! the store may have been written since the old one. A failed migration
//! rolls its one transaction back, so the store is as it was.

use std::io::{self, Write as _};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use lash_core_execution::compat::{self, CompatRefusal, CompatStamp};
use lash_core_execution::{Clock, StoreError};
use rusqlite::Transaction;
use serde::{Deserialize, Serialize};

use crate::compat::{AdvanceStep, advance_observed};
use crate::conn::SqliteConnection;
use crate::location::SqliteLocation;

/// One step of the migration catalog: the DDL that moves the database from
/// compatibility version `from` to `to`. The step's stamp write is the
/// migrator's, in the same transaction.
pub(crate) struct SqliteMigration {
    pub(crate) from: u32,
    pub(crate) to: u32,
    pub(crate) ddl: &'static str,
}

/// Phase A's synthetic successor (ADR 0115 §6) expands the database by a
/// table and a non-unique index, the SQLite twin of PostgreSQL's synthetic
/// expand. Neither constrains the writes of a build that does not know them,
/// so the build before it admits the migrated store as `Expanded`.
#[cfg(feature = "synthetic-next")]
const SYNTHETIC_NEXT_DDL: &str = "CREATE TABLE IF NOT EXISTS lash_synthetic_next (
    id INTEGER PRIMARY KEY,
    note TEXT
);
CREATE INDEX IF NOT EXISTS idx_lash_synthetic_next_note ON lash_synthetic_next(note);";

/// Every migration this build can run, in version order. 1.0 is the
/// clean-slate release, so its catalog is empty; each later release appends
/// its steps, and a database it provisions runs them too
/// ([`provisioning_steps`]). The synthetic successor's step is written
/// against the database's schema-version constant, so it follows it.
pub(crate) const CATALOG: &[SqliteMigration] = &[
    #[cfg(feature = "synthetic-next")]
    SqliteMigration {
        from: compat::SQLITE_CORE_SCHEMA_VERSION,
        to: compat::SQLITE_CORE_SCHEMA_VERSION + 1,
        ddl: SYNTHETIC_NEXT_DDL,
    },
];

/// The compatibility version this build writes for the database.
pub(crate) fn target_version() -> rusqlite::Result<u32> {
    compat::descriptor(crate::schema::COMPONENT)
        .map(|descriptor| descriptor.writes.max())
        .ok_or_else(|| crate::compat::malformed("the build has no descriptor for this database"))
}

/// The catalog DDL a database this build creates runs after its schema and
/// fragments: every step up to the version it writes.
pub(crate) fn provisioning_steps() -> impl Iterator<Item = &'static str> {
    let target = target_version().unwrap_or(0);
    CATALOG
        .iter()
        .filter(move |step| step.to <= target)
        .map(|step| step.ddl)
}

/// The catalog steps that move the database from `from` to `to`, in order,
/// or `None` when the catalog has no such path.
fn catalog_path(from: u32, to: u32) -> Option<Vec<&'static SqliteMigration>> {
    let mut steps = Vec::new();
    let mut at = from;
    while at < to {
        let step = CATALOG
            .iter()
            .find(|step| step.from == at && step.to <= to)?;
        steps.push(step);
        at = step.to;
    }
    Some(steps)
}

/// Where an open-time migration keeps the backup it takes before it changes
/// the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SqliteBackupLocation {
    /// `migration-backups/` in the directory that holds the store's
    /// database file.
    BesideStore,
    /// This directory, created if absent. Several stores may share it: each
    /// backup's manifest names the store it was taken from, and an open only
    /// ever resumes or removes its own store's backups.
    Directory(PathBuf),
}

/// How a SQLite store's open-time migration backs the store up first.
///
/// A migration copies the database into one new directory under `location`
/// before it changes it, and keeps the newest `retain` finished backups of
/// the store. A backup an interrupted migration still needs is never
/// removed. The default keeps two backups beside the store.
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

/// The directory [`SqliteBackupLocation::BesideStore`] names beside a
/// store's database file.
const BESIDE_STORE_DIRECTORY: &str = "migration-backups";
/// Every backup directory's name: this prefix and a six-digit sequence.
const BACKUP_PREFIX: &str = "sqlite-backup-";
const MANIFEST: &str = "manifest.json";
const MANIFEST_STAGING: &str = "manifest.json.staging";

/// One observable point of an open-time migration.
///
/// A migration from scratch passes `Owned`, `BackupStarted`, `Copied`,
/// `BackupSealed`, `Locked`, `Migrated`, `Committed` and `Completed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqliteMigrationStep {
    /// The database is checkpointed and closed, and no other connection
    /// holds it.
    Owned,
    /// The backup directory exists and its manifest records `backing_up`.
    BackupStarted,
    /// The database's bytes are copied into the backup and synced.
    Copied,
    /// The manifest records `migrating`: the backup is complete and durable.
    BackupSealed,
    /// `BEGIN EXCLUSIVE` holds the database.
    Locked,
    /// The catalog steps and the stamp are written, uncommitted.
    Migrated,
    /// The migration committed.
    Committed,
    /// The manifest records `migrated`.
    Completed,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum BackupState {
    /// The copy is being taken; the store is unchanged.
    BackingUp {},
    /// The backup is complete; the store may be migrated.
    Migrating {},
    /// The migration completed.
    Migrated {},
}

impl BackupState {
    fn finished(&self) -> bool {
        matches!(self, Self::Migrated {})
    }
}

/// A backup's `manifest.json`: which store it holds, what the migration it
/// belongs to was doing, and the database's copy.
/// The migration backup's self-describing recovery record.
/// version_surface = "migrate"
/// version_unguarded = "backend-private recovery file decoded before the store catalog can admit its FleetFormat; exact bootstrap reader until the release cut"
/// format_outside_manifest = "backend-private backup manifest read before store admission"
/// version_guard(roots(Manifest))
pub const SQLITE_MIGRATION_BACKUP_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    /// The store's identity ([`SqliteLocation::identity`]).
    store: String,
    #[serde(flatten)]
    state: BackupState,
    taken_at_ms: u64,
    /// The database's file name, which its copy also carries.
    file: String,
    /// The stamp's version when the copy was taken.
    from: u32,
    /// The version the migration moves it to.
    to: u32,
    /// The copy's length.
    bytes: u64,
}

/// One backup directory and its manifest.
struct Backup {
    directory: PathBuf,
    sequence: u64,
    manifest: Manifest,
}

impl Backup {
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

/// Migrate the store at `location` when its database is older than this
/// build writes, or finish what an interrupted migration started. A memory
/// store is always this process's own build's, so it never migrates.
pub(crate) async fn migrate_on_open(
    location: &SqliteLocation,
    backup: &SqliteMigrationBackup,
    busy_timeout: Duration,
    clock: &dyn Clock,
    probe: Probe,
) -> Result<(), StoreError> {
    SqliteConnection::check_release_before_open(
        &location.target(),
        crate::release_stamp::BUILD_RELEASE,
    )
    .await
    .map_err(crate::sqlite_async_error)?;
    let SqliteLocation::File { path } = location else {
        return Ok(());
    };
    let backups = match &backup.location {
        SqliteBackupLocation::BesideStore => path
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf)
            .join(BESIDE_STORE_DIRECTORY),
        SqliteBackupLocation::Directory(directory) => directory.clone(),
    };
    Migration {
        location,
        path,
        backups,
        retain: backup.retain.get(),
        busy_timeout,
        clock,
        probe,
    }
    .run()
    .await
    .map_err(Stop::into_store_error)
}

struct Migration<'a> {
    location: &'a SqliteLocation,
    /// The database file.
    path: &'a Path,
    backups: PathBuf,
    retain: usize,
    busy_timeout: Duration,
    clock: &'a dyn Clock,
    probe: Probe,
}

/// What an open found to do: nothing, or back the store up and migrate it
/// from `from` to `to`.
enum Plan {
    Nothing,
    Fresh { from: u32, to: u32 },
}

impl Migration<'_> {
    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    async fn run(&self) -> Result<(), Stop> {
        // Only a fresh store skips migration so the installer can create it.
        if !self.path.exists() {
            return Ok(());
        }
        let identity = self.location.identity();
        if self.pending(&identity)?.is_none()
            && matches!(self.plan(self.stamp().await?)?, Plan::Nothing)
        {
            return Ok(());
        }
        let _migrator = crate::store_ownership::exclusive(self.location, self.busy_timeout)
            .await
            .map_err(Stop::Failed)?;
        // Decided again under the lock: another process's migration may have
        // finished while this one waited for it.
        self.run_exclusive(identity).await
    }

    async fn run_exclusive(&self, identity: String) -> Result<(), Stop> {
        let stamp = self.stamp().await?;
        if let Some(mut pending) = self.pending(&identity)? {
            match pending.manifest.state {
                BackupState::BackingUp {} => remove_directory(&pending.directory)?,
                BackupState::Migrating {} if pending.manifest.to == target_version()? => {
                    if stamp.map(|stamp| stamp.version) == Some(pending.manifest.to) {
                        // The migration's transaction committed before the
                        // open that ran it stopped.
                        pending.record(BackupState::Migrated {})?;
                        self.probe.at(SqliteMigrationStep::Completed)?;
                        self.prune(&identity);
                        return Ok(());
                    }
                    // Nothing committed, so the store is what it was, or
                    // newer if another build wrote it since: a fresh backup
                    // replaces this one.
                    remove_directory(&pending.directory)?;
                }
                // Another build's migration: this build cannot complete it,
                // and its installer admits or refuses the stamp.
                BackupState::Migrating {} => return Ok(()),
                BackupState::Migrated {} => {}
            }
        }
        match self.plan(stamp)? {
            Plan::Nothing => Ok(()),
            Plan::Fresh { from, to } => self.fresh(identity, from, to).await,
        }
    }

    /// The database's stamp, read-only.
    async fn stamp(&self) -> Result<Option<CompatStamp>, Stop> {
        Ok(SqliteConnection::migration_stamp(&self.location.target(), self.busy_timeout).await?)
    }

    /// Migrate when the database is older than this build writes and stamped
    /// inside what this build reads. Anything else is the installer's to
    /// admit or refuse.
    fn plan(&self, stamp: Option<CompatStamp>) -> Result<Plan, Stop> {
        let Some(stamp) = stamp else {
            return Ok(Plan::Nothing);
        };
        let reads = compat::descriptor(crate::schema::COMPONENT)
            .map(|descriptor| descriptor.reads)
            .ok_or_else(|| {
                crate::compat::malformed("the build has no descriptor for this database")
            })?;
        let target = target_version()?;
        if stamp.version < reads.min() || stamp.version >= target {
            return Ok(Plan::Nothing);
        }
        if catalog_path(stamp.version, target).is_none() {
            return Err(Stop::Failed(storage(format!(
                "this build's migration catalog has no path for the database from version {} to {}",
                stamp.version, target
            ))));
        }
        Ok(Plan::Fresh {
            from: stamp.version,
            to: target,
        })
    }

    async fn fresh(&self, identity: String, from: u32, to: u32) -> Result<(), Stop> {
        self.own().await?;
        self.probe.at(SqliteMigrationStep::Owned)?;
        let mut backup = self.start_backup(identity, from, to)?;
        let sealed = (|| {
            self.probe.at(SqliteMigrationStep::BackupStarted)?;
            let copied = copy_synced(self.path, &backup.directory.join(self.file_name()))?;
            backup.manifest.bytes = copied.0;
            sync_directory(&backup.directory)?;
            self.probe.at(SqliteMigrationStep::Copied)?;
            backup.record(BackupState::Migrating {})?;
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
        self.advance(backup, copied).await
    }

    /// Migrate the database from its stamp in one exclusive transaction.
    /// `copied` is the length and modification time of the copy: a live file
    /// that changed since is refused before anything is written.
    async fn advance(&self, mut backup: Backup, copied: (u64, SystemTime)) -> Result<(), Stop> {
        let result = {
            let stopped: std::cell::RefCell<Option<Stop>> = std::cell::RefCell::new(None);
            // Preserve the migration's stop across the rusqlite boundary.
            let stop = |stop: Stop| {
                *stopped.borrow_mut() = Some(stop);
                crate::sqlite_conversion_error(storage("the migration stopped".to_owned()))
            };
            advance_observed(
                self.location,
                self.busy_timeout,
                |tx| self.migrate(&backup.manifest, tx, copied).map_err(stop),
                |step| {
                    let step = match step {
                        AdvanceStep::Locked => SqliteMigrationStep::Locked,
                        AdvanceStep::Committed => SqliteMigrationStep::Committed,
                    };
                    self.probe.at(step).map_err(stop)
                },
            )
            .map_err(|error| stopped.take().unwrap_or_else(|| Stop::from(error)))
        };
        match result {
            Ok(()) => {
                backup.record(BackupState::Migrated {})?;
                self.probe.at(SqliteMigrationStep::Completed)?;
                self.prune(&backup.manifest.store);
                Ok(())
            }
            #[cfg(feature = "testing")]
            Err(crashed @ Stop::Crashed(_)) => Err(crashed),
            Err(Stop::Failed(error)) => {
                // A transaction that did not commit changed nothing, and its
                // backup goes with the error. One that committed leaves its
                // backup `migrating` for the next open to record finished.
                if self.stamp().await?.map(|stamp| stamp.version) != Some(backup.manifest.to) {
                    remove_directory(&backup.directory)?;
                }
                Err(Stop::Failed(error))
            }
        }
    }

    fn migrate(
        &self,
        manifest: &Manifest,
        tx: &Transaction<'_>,
        copied: (u64, SystemTime),
    ) -> Result<(), Stop> {
        self.refuse_changed_since_copy(copied)?;
        let Some((stamp, _)) = crate::compat::read(tx)? else {
            return Err(Stop::Failed(StoreError::Incompatible {
                refusal: CompatRefusal::Unstamped {
                    component: crate::schema::COMPONENT.as_str().to_owned(),
                    writing_release: crate::compat::writing_release(tx),
                },
            }));
        };
        if stamp.version != manifest.from {
            return Err(Stop::Failed(storage(format!(
                "the database is at version {}, but its backup was taken at {}: another build \
                 changed it during the migration",
                stamp.version, manifest.from
            ))));
        }
        let steps = catalog_path(manifest.from, manifest.to).ok_or_else(|| {
            Stop::Failed(storage(format!(
                "this build's migration catalog has no path for the database from version {} to {}",
                manifest.from, manifest.to
            )))
        })?;
        for step in steps {
            tx.execute_batch(step.ddl)?;
        }
        let stamped = tx.execute(
            "UPDATE lash_compat SET version = ?1 WHERE singleton = 1 AND version = ?2",
            [i64::from(manifest.to), i64::from(manifest.from)],
        )?;
        if stamped != 1 {
            return Err(Stop::Failed(storage(
                "the database's stamp moved during its migration".to_owned(),
            )));
        }
        self.probe.at(SqliteMigrationStep::Migrated)
    }

    /// Under the migration's lock, the live file must be the one that was
    /// copied: the same length and modification time, and an empty
    /// write-ahead log. Anything else is another process writing the store
    /// since the copy, and the migration stops before it writes.
    fn refuse_changed_since_copy(&self, copied: (u64, SystemTime)) -> Result<(), Stop> {
        let now = file_stat(self.path)?;
        let log = file_stat(&sidecar(self.path, "-wal")).ok();
        if now != copied || log.is_some_and(|(bytes, _)| bytes != 0) {
            return Err(Stop::Failed(storage(
                "the database changed after it was backed up: another process wrote the store \
                 during its migration, which needs the store to itself"
                    .to_owned(),
            )));
        }
        Ok(())
    }

    /// Checkpoint the database and close it; succeed once the close removed
    /// the write-ahead log, which SQLite does only for the last connection. A
    /// log that stays means another connection holds the database: wait for
    /// it up to the busy timeout, then refuse.
    async fn own(&self) -> Result<(), Stop> {
        let log = sidecar(self.path, "-wal");
        let deadline = Instant::now() + self.busy_timeout;
        loop {
            SqliteConnection::checkpoint_for_migration(&self.location.target(), self.busy_timeout)
                .await?;
            if !log.exists() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Stop::Failed(StoreError::MigrationOpenElsewhere {
                    database: crate::schema::DATABASE_NAME.to_owned(),
                    location: self.path.to_path_buf(),
                }));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn start_backup(&self, store: String, from: u32, to: u32) -> Result<Backup, Stop> {
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
                format: lash_core_store::store::FleetFormat::seed(
                    lash_core_store::store::FLEET_WRITABLE_RANGE,
                )
                .writer_version(lash_core_store::surface_format!(
                    SQLITE_MIGRATION_BACKUP_VERSION
                )),
                store,
                state: BackupState::BackingUp {},
                taken_at_ms: self.clock.timestamp_ms(),
                file: self.file_name(),
                from,
                to,
                bytes: 0,
            },
        };
        backup.record(BackupState::BackingUp {})?;
        Ok(backup)
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

    /// This store's readable backups, oldest first; unreadable recovery records refuse.
    fn store_backups(&self, identity: &str) -> Result<Vec<Backup>, Stop> {
        let mut backups = Vec::new();
        for (sequence, directory) in self.backup_directories()? {
            let path = directory.join(MANIFEST);
            // An absent manifest is an unsealed directory; an unreadable one
            // may own an interrupted migration and must refuse admission.
            if !path
                .try_exists()
                .map_err(|error| io_failure(path.display(), error))?
            {
                continue;
            }
            let bytes = read_file(&path).map_err(|error| io_failure(path.display(), error))?;
            #[derive(Deserialize)]
            struct Stamp {
                format: u32,
            }
            let stamp: Stamp = serde_json::from_slice(&bytes).map_err(|error| {
                Stop::Failed(StoreError::Incompatible {
                    refusal: lash_core_execution::compat::CompatRefusal::MalformedStamp {
                        component: path.display().to_string(),
                        detail: error.to_string(),
                        writing_release: None,
                    },
                })
            })?;
            if stamp.format != SQLITE_MIGRATION_BACKUP_VERSION {
                return Err(Stop::Failed(StoreError::Incompatible {
                    refusal: lash_core_execution::compat::CompatRefusal::UnknownVocabulary {
                        surface: format!("SQLite migration backup format at {}", path.display()),
                        label: stamp.format.to_string(),
                    },
                }));
            }
            let manifest = serde_json::from_slice::<Manifest>(&bytes).map_err(|error| {
                Stop::Failed(StoreError::Incompatible {
                    refusal: lash_core_execution::compat::CompatRefusal::MalformedStamp {
                        component: path.display().to_string(),
                        detail: error.to_string(),
                        writing_release: None,
                    },
                })
            })?;
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

    /// The newest of this store's backups whose migration has not finished.
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

// The filesystem work of a backup. The store's host supplies the database
// file and the backup location; these touch only files there.

#[expect(
    clippy::disallowed_methods,
    reason = "a migration copies the host's database file into the host-configured backup location"
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
    reason = "a migration stats the host's database file it copied"
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
    reason = "a migration renames its manifest under the host-configured backup location"
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
    reason = "a migration syncs the host's backup directories after it creates or renames in them"
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
mod nested_format_tests {
    #[test]
    fn backup_states_carry_no_failure_data() {
        for state in [
            super::BackupState::BackingUp {},
            super::BackupState::Migrating {},
            super::BackupState::Migrated {},
        ] {
            let manifest = super::Manifest {
                format: super::SQLITE_MIGRATION_BACKUP_VERSION,
                store: "store".into(),
                state,
                taken_at_ms: 0,
                file: "lash.db".into(),
                from: 1,
                to: 2,
                bytes: 0,
            };
            let encoded = serde_json::to_vec(&manifest).expect("manifest");
            assert!(serde_json::from_slice::<super::Manifest>(&encoded).is_ok());
        }
        for state in ["backing_up", "migrating", "migrated"] {
            assert!(
                serde_json::from_value::<super::BackupState>(
                    serde_json::json!({"state":state, "failure":"failed"})
                )
                .is_err()
            );
        }
    }

    #[tokio::test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the law corrupts its own backup manifest"
    )]
    async fn unreadable_backup_manifest_refuses_open() {
        let root = tempfile::tempdir().expect("store root");
        let database = root.path().join("lash.db");
        drop(
            crate::SqliteStoreSet::open(&database)
                .await
                .expect("provision"),
        );
        let backup = crate::location::canonical_path(root.path())
            .join("migration-backups/sqlite-backup-000001");
        std::fs::create_dir_all(&backup).expect("backup directory");
        for bytes in [
            b"{".as_slice(),
            br#"{"format":4294967295}"#.as_slice(),
            br#"{"format":1,"state":"migrating"}"#.as_slice(),
        ] {
            std::fs::write(backup.join("manifest.json"), bytes).expect("unreadable manifest");
            let error = match crate::SqliteStoreSet::open(&database).await {
                Err(error) => crate::sqlite_async_error(error),
                Ok(_) => panic!("unreadable recovery record was admitted"),
            };
            assert!(
                matches!(error, crate::StoreError::Incompatible { .. }),
                "{error}"
            );
        }
    }
}
