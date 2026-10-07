//! Read the recorded schema versions of a SQLite deployment without opening it.
//!
//! PostgreSQL has had [`verify_schema_for`] since ADR 0052: a check that reads
//! a database too broken to open, which is most of the ones worth reading.
//! SQLite had no equivalent, and the gap was not cosmetic. Its open path takes
//! `BEGIN IMMEDIATE` *before* reading `lash_compat`
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

use std::path::Path;

use async_trait::async_trait;
use lash_core_execution::{
    DurableScan, DurableScanPage, FleetFormatState, StoreBackend, StoreError, StorePreflight,
    StoreReleaseState, StoreSchemaDatabase, StoreSchemaStatus, StoreSchemaVerdict,
};

pub(crate) mod walk;

use crate::conn::SqliteConnection;
use crate::location::{DatabaseTarget, SqliteLocation};
use crate::schema::COMPONENT;

/// Read a SQLite database's recorded schema version and compare it against
/// this build, without opening the store.
///
/// This is SQLite's counterpart to `PostgresStorage::verify_schema_for`: it
/// never provisions, never migrates, never stamps a version, and never fails on
/// drift. Every older generation is a refusal at the BLAKE3 boundary. A database that exists but cannot
/// be read yields [`StoreSchemaVerdict::Unreadable`] carrying SQLite's own
/// words, because an unreadable database is undecided rather than refused.
pub async fn verify_schema_at(path: &Path) -> StoreSchemaDatabase {
    verify_schema_target(
        &DatabaseTarget::File(path.to_path_buf()),
        crate::release_stamp::BUILD_RELEASE,
    )
    .await
}

/// The store's report row, read through its location-derived target.
async fn verify_schema_target(target: &DatabaseTarget, build_release: &str) -> StoreSchemaDatabase {
    let (verdict, min_reader) = read_compat_verdict(target, build_release).await;
    StoreSchemaDatabase {
        name: crate::schema::DATABASE_NAME.to_string(),
        location: target.to_string(),
        expected: crate::schema::expected_version(),
        min_reader,
        verdict,
    }
}

/// Inspect the authoritative compatibility row without changing the file.
async fn read_compat_verdict(
    target: &DatabaseTarget,
    build_release: &str,
) -> (StoreSchemaVerdict, Option<i64>) {
    if !target.exists() {
        return (StoreSchemaVerdict::Absent, None);
    }
    // A failed read-only open is an undecided database, never a reason to reach for a
    // connection that can write.
    let conn = match SqliteConnection::open_readonly(target).await {
        Ok(conn) => conn,
        Err(error) => {
            return (
                StoreSchemaVerdict::Unreadable {
                    reason: error.to_string(),
                },
                None,
            );
        }
    };
    let build_release = build_release.to_owned();
    let probe = conn
        .call(move |c| {
            // The engine enforces the promise the module documents: any
            // statement that would write fails here, including the implicit
            // ones a pragma could trigger.
            c.pragma_update(None, "query_only", true)?;
            let release = crate::compat::writing_release(c);
            if let Some(refusal) = lash_core_execution::compat::CompatRefusal::pre_release(
                COMPONENT.as_str(),
                release.as_deref(),
                &build_release,
            ) {
                return Ok((StoreSchemaVerdict::Refused { refusal }, None));
            }
            let Some((stamp, fleet)) = crate::compat::read(c)? else {
                return if crate::schema::has_user_schema_objects(c)? {
                    Ok((
                        StoreSchemaVerdict::Refused {
                            refusal: lash_core_execution::compat::CompatRefusal::Unstamped {
                                component: COMPONENT.as_str().to_owned(),
                                writing_release: None,
                            },
                        },
                        None,
                    ))
                } else {
                    Ok((StoreSchemaVerdict::Absent, None))
                };
            };
            let descriptor = lash_core_execution::compat::descriptor(COMPONENT)
                .ok_or_else(|| rusqlite::Error::InvalidQuery)?;
            let floor = Some(i64::from(stamp.min_reader));
            let admission = match lash_core_execution::compat::admit(
                descriptor,
                lash_core_execution::compat::StampRead::Present(stamp),
            ) {
                Ok(admission) => admission,
                Err(refusal) => return Ok((StoreSchemaVerdict::Refused { refusal }, floor)),
            };
            if let Err(error) = lash_core_execution::FleetFormat::admit(
                fleet,
                lash_core_execution::FleetFormat::writable(),
            ) {
                return Ok((
                    match error {
                        StoreError::Incompatible { refusal } => {
                            StoreSchemaVerdict::Refused { refusal }
                        }
                        error => StoreSchemaVerdict::Unreadable {
                            reason: error.to_string(),
                        },
                    },
                    floor,
                ));
            }
            let verdict = match admission {
                lash_core_execution::compat::CompatAdmission::Expanded { .. } => {
                    match crate::compat::verify_tolerant(c) {
                        Ok(()) => StoreSchemaVerdict::Expanded {
                            found: i64::from(stamp.version),
                        },
                        Err(rusqlite::Error::ToSqlConversionFailure(source)) => {
                            match source.downcast_ref::<StoreError>() {
                                Some(StoreError::Incompatible { refusal }) => {
                                    StoreSchemaVerdict::Refused {
                                        refusal: refusal.clone(),
                                    }
                                }
                                _ => StoreSchemaVerdict::Unreadable {
                                    reason: source.to_string(),
                                },
                            }
                        }
                        Err(error) => StoreSchemaVerdict::Unreadable {
                            reason: error.to_string(),
                        },
                    }
                }
                lash_core_execution::compat::CompatAdmission::Native
                    if stamp.version < crate::migration::target_version()? =>
                {
                    StoreSchemaVerdict::Migratable {
                        found: i64::from(stamp.version),
                    }
                }
                _ => StoreSchemaVerdict::Matches,
            };
            Ok((verdict, floor))
        })
        .await;
    probe.unwrap_or_else(|error| {
        (
            StoreSchemaVerdict::Unreadable {
                reason: error.to_string(),
            },
            None,
        )
    })
}

/// Read the release stamp the database carries, read-only.
///
/// An absent database reports [`StoreReleaseState::Unstamped`] on purpose: the
/// deployment records no writing release, and that is the same answer a host
/// needs whether nothing has been provisioned yet or a pre-stamp build wrote
/// it. A database that exists but cannot be read is
/// [`StoreReleaseState::Unreadable`] instead — an undecided stamp is not an
/// absent one.
async fn read_release_state(target: &DatabaseTarget) -> StoreReleaseState {
    if !target.exists() {
        return StoreReleaseState::Unstamped;
    }
    let conn = match SqliteConnection::open_readonly(target).await {
        Ok(conn) => conn,
        Err(err) => {
            return StoreReleaseState::Unreadable {
                reason: err.to_string(),
            };
        }
    };
    let read = conn
        .call(|c| {
            c.pragma_update(None, "query_only", true)?;
            crate::release_stamp::read(c)
        })
        .await;
    match read {
        Ok(state) => state,
        Err(err) => StoreReleaseState::Unreadable {
            reason: err.to_string(),
        },
    }
}

/// Read the fleet-format row the database carries, read-only.
///
/// Same discipline as [`read_release_state`]: an absent database or an absent
/// row is [`FleetFormatState::Unrecorded`] — the deployment records no fleet
/// format — and a database that exists but cannot be read is
/// [`FleetFormatState::Unreadable`], never silently absent.
async fn read_fleet_format_state(target: &DatabaseTarget) -> FleetFormatState {
    if !target.exists() {
        return FleetFormatState::Unrecorded;
    }
    let conn = match SqliteConnection::open_readonly(target).await {
        Ok(conn) => conn,
        Err(err) => {
            return FleetFormatState::Unreadable {
                reason: err.to_string(),
            };
        }
    };
    let read = conn
        .call(|c| {
            c.pragma_update(None, "query_only", true)?;
            crate::compat::read_fleet_state(c)
        })
        .await;
    match read {
        Ok(state) => state,
        Err(err) => FleetFormatState::Unreadable {
            reason: err.to_string(),
        },
    }
}

/// A read-only handle over a SQLite store, built from the same
/// [`SqliteLocation`] the open path opens.
///
/// Construction opens nothing: the location is recorded and read only when
/// [`StorePreflight::schema_status`] is called, so the probe answers for
/// exactly the database `SqliteStoreSet::open` would open.
#[derive(Clone, Debug)]
pub struct SqliteStorePreflight {
    location: SqliteLocation,
}

impl SqliteStorePreflight {
    /// The store at `location`, the same value `SqliteStoreSet` keeps.
    ///
    /// A file path is canonicalized the way the open path canonicalizes it,
    /// so the report's identity matches the store set's.
    pub fn for_location(location: SqliteLocation) -> Self {
        let location = match location {
            SqliteLocation::File { path } => SqliteLocation::File {
                path: crate::location::canonical_path(&path),
            },
            memory => memory,
        };
        Self { location }
    }

    /// The file store in the database file at `path`: the same path
    /// [`SqliteStoreSet::open`](crate::SqliteStoreSet::open) takes.
    pub fn for_database_file(path: impl Into<std::path::PathBuf>) -> Self {
        Self::for_location(SqliteLocation::File { path: path.into() })
    }
}

#[async_trait]
impl StorePreflight for SqliteStorePreflight {
    fn backend(&self) -> StoreBackend {
        StoreBackend::Sqlite {
            location: match &self.location {
                SqliteLocation::File { path } => path.display().to_string(),
                SqliteLocation::Memory { id } => format!("memory:{id}"),
            },
        }
    }

    async fn schema_status(&self) -> Result<StoreSchemaStatus, StoreError> {
        let target = self.location.target();
        let release = read_release_state(&target).await;
        let fleet_format = read_fleet_format_state(&target).await;
        let row = verify_schema_target(&target, crate::release_stamp::BUILD_RELEASE).await;
        Ok(StoreSchemaStatus {
            databases: vec![row],
            release,
            fleet_format,
        })
    }

    /// Walk one page of one durable surface. See [`walk`] for the read-only
    /// discipline, the keyset cursors, and why nothing there decodes a payload.
    async fn scan_durable(&self, scan: &DurableScan) -> Result<DurableScanPage, StoreError> {
        walk::scan_durable(self, scan).await
    }
}

#[cfg(test)]
mod tests;
