//! Read the recorded schema versions of a SQLite deployment without opening it.
//!
//! PostgreSQL has had [`verify_schema_for`] since ADR 0052: a check that reads
//! a database too broken to open, which is most of the ones worth reading.
//! SQLite had no equivalent, and the gap was not cosmetic. Its open path takes
//! `BEGIN IMMEDIATE` *before* reading `PRAGMA user_version`
//! (`schema::prepare_versioned_schema`) — deliberately, because a
//! read-then-upgrade races concurrent first-openers into a lock-upgrade
//! deadlock — and it opens with `SQLITE_OPEN_CREATE`, so an open that is going
//! to be refused still takes the write lock and still creates the file. Under a
//! supervisor that is the crash loop this module exists to replace: the host
//! could not ask "will this open?" without performing most of the open.
//!
//! Everything here reads, and the connection is built so that it cannot do
//! otherwise. A preflight connection is opened `SQLITE_OPEN_READ_ONLY` with no
//! `SQLITE_OPEN_CREATE`, so a missing database is reported as
//! [`StoreSchemaVerdict::Absent`] rather than created; the version read takes
//! SQLite's shared lock only, so a writer holding the write lock does not delay
//! the answer; and `PRAGMA query_only` is set before anything is read, which
//! makes the read-only promise an invariant the engine enforces rather than a
//! property of the statements this module happens to send.
//!
//! **What a read-only connection may still touch, stated rather than hidden.**
//! Opening a WAL database read-only can create its `-shm`/`-wal` sidecars, which
//! are recoverable index files rather than durable content — no schema is
//! applied, no version is stamped, no row is written, and the database file
//! itself is never brought into existence. What this path deliberately does
//! *not* do is fall back to a read-write connection when the read-only open
//! fails: a read-write connection checkpoints a hot WAL and deletes it on close,
//! which rewrites the main database file's bytes. That is a write, inside a
//! surface whose entire value is that it is not one, so an open this path cannot
//! perform read-only is reported as [`StoreSchemaVerdict::Unreadable`] instead.
//!
//! [`verify_schema_for`]: https://docs.rs/lash-postgres-store

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lash_core::{
    DurableScan, DurableScanPage, StoreBackend, StoreError, StorePreflight, StoreSchemaDatabase,
    StoreSchemaStatus, StoreSchemaVerdict,
};

mod walk;

use crate::conn::SqliteConnection;
use crate::schema::{
    EFFECT_SCHEMA_VERSION, PROCESS_SCHEMA_VERSION, SCHEMA_VERSION, TRIGGER_SCHEMA_VERSION,
};

/// One of the four independently versioned SQLite databases a lash deployment
/// can hold.
///
/// They version separately and can disagree, which is why a refusal has to name
/// the database rather than the deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SqliteDatabase {
    /// Sessions, graph nodes, checkpoints, leases, queued work.
    DurableCore,
    /// The process registry.
    ProcessRegistry,
    /// The trigger store.
    Triggers,
    /// The effect-replay journal and await-event ledger.
    EffectReplay,
}

impl SqliteDatabase {
    /// The `PRAGMA user_version` this build requires of the database.
    ///
    /// These are the same integers the open path enforces; reading one here and
    /// finding a different one on disk is exactly the refusal an open would
    /// have produced.
    pub fn expected_version(self) -> i64 {
        i64::from(match self {
            SqliteDatabase::DurableCore => SCHEMA_VERSION,
            SqliteDatabase::ProcessRegistry => PROCESS_SCHEMA_VERSION,
            SqliteDatabase::Triggers => TRIGGER_SCHEMA_VERSION,
            SqliteDatabase::EffectReplay => EFFECT_SCHEMA_VERSION,
        })
    }

    /// The operator-facing name used in reports and refusal messages.
    pub fn name(self) -> &'static str {
        match self {
            SqliteDatabase::DurableCore => "durable core",
            SqliteDatabase::ProcessRegistry => "process registry",
            SqliteDatabase::Triggers => "trigger store",
            SqliteDatabase::EffectReplay => "effect replay",
        }
    }
}

/// Read one SQLite database's recorded schema version and compare it against
/// this build, without opening the store.
///
/// This is SQLite's counterpart to `PostgresStorage::verify_schema_for`: it
/// never provisions, never migrates, never stamps a version, and never fails on
/// drift — a mismatch is the returned verdict, not an error. A database that
/// exists but cannot be read yields [`StoreSchemaVerdict::Unreadable`] carrying
/// SQLite's own words, because an unreadable database is undecided rather than
/// refused.
pub async fn verify_schema_at(path: &Path, database: SqliteDatabase) -> StoreSchemaDatabase {
    let verdict = read_user_version(path).await;
    let verdict = match verdict {
        Ok(None) => StoreSchemaVerdict::Absent,
        Ok(Some(found)) if found == database.expected_version() => StoreSchemaVerdict::Matches,
        Ok(Some(found)) => StoreSchemaVerdict::Mismatch { found },
        Err(reason) => StoreSchemaVerdict::Unreadable { reason },
    };
    StoreSchemaDatabase {
        name: database.name().to_string(),
        location: path.display().to_string(),
        expected: database.expected_version(),
        verdict,
    }
}

/// `Ok(None)` when nothing is provisioned: either the file is absent, or it
/// exists with `user_version` 0 and no user objects, which is precisely the
/// state the open path would initialise.
async fn read_user_version(path: &Path) -> Result<Option<i64>, String> {
    if !path.exists() {
        return Ok(None);
    }
    // Read-only or not at all. A failed read-only open is an undecided
    // database, never a reason to reach for a connection that can write.
    let conn = SqliteConnection::open_readonly(path)
        .await
        .map_err(|err| err.to_string())?;
    let probe = conn
        .call(|c| {
            // The engine enforces the promise the module documents: any
            // statement that would write fails here, including the implicit
            // ones a pragma could trigger.
            c.pragma_update(None, "query_only", true)?;
            let user_version: i64 = c.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if user_version != 0 {
                return Ok(Some(user_version));
            }
            // Version 0 over an existing schema is not "empty"; it is a
            // database the open path would refuse. Distinguish the two here
            // rather than reporting a fresh file as a live deployment.
            if crate::schema::has_user_schema_objects(c)? {
                Ok(Some(0))
            } else {
                Ok(None)
            }
        })
        .await;
    probe.map_err(|err| err.to_string())
}

/// A read-only handle over a SQLite deployment, built from the same paths a
/// host hands its factories.
///
/// Construction opens nothing: the paths are recorded and read only when
/// [`StorePreflight::schema_status`] is called. A deployment that wires only
/// some of the four databases declares only those, and the report says so —
/// naming what was not inspected is part of the answer.
#[derive(Clone, Debug)]
pub struct SqliteStorePreflight {
    durable_core: PathBuf,
    process_registry: Option<PathBuf>,
    triggers: Option<PathBuf>,
    effect_replay: Option<PathBuf>,
}

impl SqliteStorePreflight {
    /// Build a preflight over the session-store root a
    /// [`SqliteSessionStoreFactory`](crate::SqliteSessionStoreFactory) would be
    /// constructed from. The durable-core database is derived from it the same
    /// way the factory derives it.
    pub fn for_session_store_root(root: impl Into<PathBuf>) -> Self {
        Self {
            durable_core: root.into().join(crate::DURABLE_CORE_DB_FILE),
            process_registry: None,
            triggers: None,
            effect_replay: None,
        }
    }

    /// Build a preflight over an explicit durable-core database file.
    pub fn for_durable_core(path: impl Into<PathBuf>) -> Self {
        Self {
            durable_core: path.into(),
            process_registry: None,
            triggers: None,
            effect_replay: None,
        }
    }

    /// Also read the process-registry database at the path the host passes to
    /// [`SqliteProcessRegistry::open`](crate::SqliteProcessRegistry::open).
    pub fn with_process_registry(mut self, path: impl Into<PathBuf>) -> Self {
        self.process_registry = Some(path.into());
        self
    }

    /// Also read the trigger database at the path the host passes to
    /// [`SqliteTriggerStore::open`](crate::SqliteTriggerStore::open).
    pub fn with_trigger_store(mut self, path: impl Into<PathBuf>) -> Self {
        self.triggers = Some(path.into());
        self
    }

    /// Also read the effect-replay journal at the path the host passes to its
    /// effect host.
    pub fn with_effect_journal(mut self, path: impl Into<PathBuf>) -> Self {
        self.effect_replay = Some(path.into());
        self
    }

    fn declared(&self) -> Vec<(SqliteDatabase, &Path)> {
        let mut declared: Vec<(SqliteDatabase, &Path)> =
            vec![(SqliteDatabase::DurableCore, self.durable_core.as_path())];
        if let Some(path) = &self.process_registry {
            declared.push((SqliteDatabase::ProcessRegistry, path.as_path()));
        }
        if let Some(path) = &self.triggers {
            declared.push((SqliteDatabase::Triggers, path.as_path()));
        }
        if let Some(path) = &self.effect_replay {
            declared.push((SqliteDatabase::EffectReplay, path.as_path()));
        }
        declared
    }
}

#[async_trait]
impl StorePreflight for SqliteStorePreflight {
    fn backend(&self) -> StoreBackend {
        StoreBackend::Sqlite {
            location: self.durable_core.display().to_string(),
        }
    }

    async fn schema_status(&self) -> Result<StoreSchemaStatus, StoreError> {
        let mut databases = Vec::new();
        for (database, path) in self.declared() {
            databases.push(verify_schema_at(path, database).await);
        }
        Ok(StoreSchemaStatus { databases })
    }

    /// Walk one page of one durable surface. See [`walk`] for the read-only
    /// discipline, the keyset cursors, and why nothing there decodes a payload.
    async fn scan_durable(&self, scan: &DurableScan) -> Result<DurableScanPage, StoreError> {
        walk::scan_durable(self, scan).await
    }
}

#[cfg(test)]
mod tests;
