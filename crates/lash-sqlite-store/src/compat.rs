//! The compatibility row in each SQLite database.

use lash_core_execution::compat::{
    self, CompatAdmission, CompatRefusal, CompatStamp, StampRead, VersionRange,
};
use lash_core_execution::store::fleet_finalize::FleetEpochFlip;
use lash_core_execution::{FleetFormat, FleetFormatState, StoreError};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::SqliteDatabase;
use crate::location::SqliteLocation;

fn incompatible(refusal: CompatRefusal) -> rusqlite::Error {
    crate::sqlite_conversion_error(StoreError::Incompatible { refusal })
}

pub(crate) fn writing_release(conn: &Connection, database: SqliteDatabase) -> Option<String> {
    if database != SqliteDatabase::DurableCore {
        return None;
    }
    crate::release_stamp::read_release(conn)
}

pub(crate) fn malformed(database: SqliteDatabase, detail: impl Into<String>) -> rusqlite::Error {
    incompatible(CompatRefusal::MalformedStamp {
        component: database.component().as_str().to_owned(),
        detail: detail.into(),
        writing_release: None,
    })
}

fn malformed_on(
    conn: &Connection,
    database: SqliteDatabase,
    detail: impl Into<String>,
) -> rusqlite::Error {
    incompatible(CompatRefusal::MalformedStamp {
        component: database.component().as_str().to_owned(),
        detail: detail.into(),
        writing_release: writing_release(conn, database),
    })
}

fn table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'lash_compat')",
        [],
        |row| row.get(0),
    )
}

/// Read one database's complete stamp. No row or table is absence, never a
/// defaulted current version.
pub(crate) fn read(
    conn: &Connection,
    database: SqliteDatabase,
) -> rusqlite::Result<Option<(CompatStamp, u32)>> {
    if !table_exists(conn)? {
        return Ok(None);
    }
    let row: Option<(String, i64, i64, i64)> = conn
        .query_row(
            "SELECT component, version, min_reader, fleet_format FROM lash_compat WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|error| malformed_on(conn, database, error.to_string()))?;
    let Some((component, version, min_reader, fleet_format)) = row else {
        return Ok(None);
    };
    if component != database.component().as_str() {
        return Err(malformed_on(
            conn,
            database,
            format!("component is {component}"),
        ));
    }
    let version =
        u32::try_from(version).map_err(|error| malformed_on(conn, database, error.to_string()))?;
    let min_reader = u32::try_from(min_reader)
        .map_err(|error| malformed_on(conn, database, error.to_string()))?;
    let fleet_format = u32::try_from(fleet_format)
        .map_err(|error| malformed_on(conn, database, error.to_string()))?;
    Ok(Some((
        CompatStamp {
            version,
            min_reader,
        },
        fleet_format,
    )))
}

pub(crate) fn admit(
    conn: &Connection,
    database: SqliteDatabase,
    writable: VersionRange,
) -> rusqlite::Result<(CompatAdmission, FleetFormat)> {
    let row = read(conn, database)?;
    let stamp = row.map_or_else(
        || StampRead::Absent {
            populated: crate::schema::has_user_schema_objects(conn).unwrap_or(true),
        },
        |(stamp, _)| StampRead::Present(stamp),
    );
    let descriptor = compat::descriptor(database.component()).ok_or_else(|| {
        malformed_on(
            conn,
            database,
            "the build has no descriptor for this database",
        )
    })?;
    let release = writing_release(conn, database);
    let admission = compat::admit(descriptor, stamp)
        .map_err(|refusal| incompatible(refusal.with_writing_release(release.clone())))?;
    let fleet = match row {
        Some((_, fleet)) => FleetFormat::admit(fleet, writable).map_err(|error| match error {
            StoreError::Incompatible { refusal } => {
                incompatible(refusal.with_writing_release(release))
            }
            other => crate::sqlite_conversion_error(other),
        })?,
        None => FleetFormat::current(),
    };
    Ok((admission, fleet))
}

/// The writer fence's one statement (ADR 0115 §2.2).
const FENCE: &str =
    "SELECT component, version, min_reader, fleet_format FROM lash_compat WHERE singleton = 1";

/// The writer fence, the first statement of every write transaction after
/// `BEGIN IMMEDIATE` (ADR 0115 §2.2). The reserved lock that `BEGIN
/// IMMEDIATE` holds keeps any other writer, and any migration or finalize,
/// from changing the row until the transaction ends.
///
/// It answers the epoch `F` the transaction runs under. It also re-admits the
/// component stamp, because another process can migrate a shared database
/// while this one holds a connection: a raised floor refuses typed here, not
/// at the next open. A missing or malformed row fails closed. `F` outside
/// `writable` is the terminal [`StoreError::WriterFenced`]; the caller's
/// rollback leaves the transaction having written nothing.
pub(crate) fn fence(
    conn: &Connection,
    database: SqliteDatabase,
    writable: VersionRange,
) -> rusqlite::Result<FleetFormat> {
    let row: Option<(String, i64, i64, i64)> = conn
        .prepare_cached(FENCE)
        .and_then(|mut statement| {
            statement
                .query_row([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .optional()
        })
        .map_err(|error| malformed_on(conn, database, error.to_string()))?;
    let Some((component, version, min_reader, fleet)) = row else {
        return Err(incompatible(CompatRefusal::Unstamped {
            component: database.component().as_str().to_owned(),
            writing_release: writing_release(conn, database),
        }));
    };
    let descriptor = compat::descriptor(database.component()).ok_or_else(|| {
        malformed_on(
            conn,
            database,
            "the build has no descriptor for this database",
        )
    })?;
    if component != database.component().as_str() {
        return Err(malformed_on(
            conn,
            database,
            format!("component is {component}"),
        ));
    }
    let stamp = CompatStamp {
        version: u32::try_from(version)
            .map_err(|error| malformed_on(conn, database, error.to_string()))?,
        min_reader: u32::try_from(min_reader)
            .map_err(|error| malformed_on(conn, database, error.to_string()))?,
    };
    compat::admit(descriptor, StampRead::Present(stamp)).map_err(|refusal| {
        incompatible(refusal.with_writing_release(writing_release(conn, database)))
    })?;
    let fleet =
        u32::try_from(fleet).map_err(|error| malformed_on(conn, database, error.to_string()))?;
    FleetFormat::fence(fleet, writable).map_err(crate::sqlite_conversion_error)
}

/// One step of [`advance_set`], as its observer sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdvanceStep {
    /// `BEGIN EXCLUSIVE` holds this database.
    Locked(SqliteDatabase),
    /// This database's rewrite committed.
    Committed(SqliteDatabase),
}

/// Advance the whole store: a migration or a finalize (ADR 0115 §2.2).
///
/// It takes `BEGIN EXCLUSIVE` on every database in [`SqliteDatabase::ALL`]
/// order, and only once it holds all three does `rewrite` change each one
/// (its DDL and its `lash_compat` row). It then commits in the same order.
/// While it holds a database no writer there passes its fence, and a writer
/// that was already past its fence finishes first under the old row. A crash
/// between two commits leaves the databases disagreeing, and the next set
/// open refuses that as `PartiallyAdvanced` ([`check_set`]) unless the
/// opening build's migrations complete the set forward
/// ([`crate::migration`]).
pub(crate) fn advance_set(
    location: &SqliteLocation,
    busy_timeout: std::time::Duration,
    rewrite: impl FnMut(SqliteDatabase, &Transaction<'_>) -> rusqlite::Result<()>,
) -> rusqlite::Result<()> {
    advance_set_observed(location, busy_timeout, rewrite, |_| Ok(()))
}

/// [`advance_set`], reporting each lock and commit to `observe` as it
/// happens. An error from `observe` stops the advance there: every
/// transaction not yet committed rolls back, as a crash at that point would.
pub(crate) fn advance_set_observed(
    location: &SqliteLocation,
    busy_timeout: std::time::Duration,
    mut rewrite: impl FnMut(SqliteDatabase, &Transaction<'_>) -> rusqlite::Result<()>,
    mut observe: impl FnMut(AdvanceStep) -> rusqlite::Result<()>,
) -> rusqlite::Result<()> {
    let mut connections = Vec::with_capacity(SqliteDatabase::ALL.len());
    for database in SqliteDatabase::ALL {
        let connection = Connection::open_with_flags(
            location.target(database).uri(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        connection.busy_timeout(busy_timeout)?;
        connections.push((database, connection));
    }
    let mut held = Vec::with_capacity(connections.len());
    for (database, connection) in &mut connections {
        held.push((
            *database,
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)?,
        ));
        observe(AdvanceStep::Locked(*database))?;
    }
    for (database, tx) in &held {
        rewrite(*database, tx)?;
    }
    for (database, tx) in held {
        tx.commit()?;
        observe(AdvanceStep::Committed(database))?;
    }
    Ok(())
}

/// Finalize: move `F` in every database of the store to `writable.max()`,
/// the finalizing build's `F_self`, as one [`advance_set`] (ADR 0106 §2, ADR
/// 0115 §2.2). Every writer whose writable range excludes the new `F` is
/// fenced from its next transaction on.
///
/// Each database's row passes the writer fence first, under the exclusive
/// lock: its stamp is re-admitted, and a recorded `F` outside `writable` is
/// `WriterFenced`, so a build a newer release already fenced cannot finalize.
/// A database already at `F_self` is left as it is, which completes forward
/// a set a crash left partially finalized. The answer is `Finalized` from
/// the lowest epoch any database recorded, or `AlreadyFinalized` when every
/// database already records `F_self`.
pub(crate) fn finalize(
    location: &SqliteLocation,
    busy_timeout: std::time::Duration,
    writable: VersionRange,
) -> rusqlite::Result<FleetEpochFlip> {
    let target = writable.max();
    let mut lowest = target;
    advance_set(location, busy_timeout, |database, tx| {
        let recorded = fence(tx, database, writable)?.version();
        lowest = lowest.min(recorded);
        if recorded == target {
            return Ok(());
        }
        tx.execute(
            "UPDATE lash_compat SET fleet_format = ?1 WHERE singleton = 1",
            [i64::from(target)],
        )?;
        Ok(())
    })?;
    Ok(if lowest == target {
        FleetEpochFlip::AlreadyFinalized { fleet: target }
    } else {
        FleetEpochFlip::Finalized {
            from: lowest,
            to: target,
        }
    })
}

/// Only the installer writes the row. An existing row is never changed by an
/// ordinary open, including an open by an older build after expand.
///
/// `F` starts at [`FleetFormat::seed`] of the provisioning build's writable
/// range `writable`, its floor: a compatibility release that provisions a
/// database leaves it inside the rollback window, and only finalize moves `F`
/// (ADR 0115 §2.1).
pub(crate) fn provision(
    tx: &Transaction<'_>,
    database: SqliteDatabase,
    writable: VersionRange,
) -> rusqlite::Result<()> {
    let descriptor = compat::descriptor(database.component())
        .ok_or_else(|| malformed(database, "the build has no descriptor for this database"))?;
    tx.execute(
        "INSERT INTO lash_compat (singleton, component, version, min_reader, fleet_format)
         VALUES (1, ?1, ?2, ?3, ?4)",
        params![
            descriptor.component.as_str(),
            descriptor.writes.max(),
            descriptor.reads.min(),
            FleetFormat::seed(writable).version(),
        ],
    )?;
    Ok(())
}

/// Refuse a database this build reads but has not migrated: its stamp is
/// older than the version the build writes. Only the store's open migrates,
/// every database together after a complete backup ([`crate::migration`]);
/// a component's installer never does.
pub(crate) fn refuse_unmigrated(
    conn: &Connection,
    database: SqliteDatabase,
) -> rusqlite::Result<()> {
    let Some((stamp, _)) = read(conn, database)? else {
        return Ok(());
    };
    let target = crate::migration::target_version(database)?;
    if stamp.version < target {
        return Err(incompatible(CompatRefusal::MigrationPending {
            component: database.component().as_str().to_owned(),
            found: stamp.version,
            target,
            writing_release: writing_release(conn, database),
        }));
    }
    Ok(())
}

pub(crate) fn read_recorded(
    conn: &Connection,
    writable: VersionRange,
) -> rusqlite::Result<FleetFormat> {
    let (_, fleet) = admit(conn, SqliteDatabase::DurableCore, writable)?;
    Ok(fleet)
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn recorded_or_current(conn: &Connection) -> rusqlite::Result<FleetFormat> {
    read_recorded(conn, FleetFormat::writable())
}

pub(crate) fn read_fleet_state(conn: &Connection) -> rusqlite::Result<FleetFormatState> {
    match read(conn, SqliteDatabase::DurableCore) {
        Ok(Some((_, fleet))) => Ok(FleetFormatState::Recorded(FleetFormat::from_version(fleet))),
        Ok(None) => Ok(FleetFormatState::Unrecorded),
        Err(error) => Ok(FleetFormatState::Unreadable {
            reason: error.to_string(),
        }),
    }
}

/// Detect a crash between the three independent database commits before any
/// component open can mistake the set for a consistent fleet epoch.
pub(crate) fn check_set(location: &SqliteLocation) -> rusqlite::Result<()> {
    let mut rows = Vec::new();
    let mut release = None;
    for database in SqliteDatabase::ALL {
        let target = location.target(database);
        if target.file_path().is_some_and(|path| !path.exists()) {
            return Ok(());
        }
        let conn = Connection::open_with_flags(
            target.uri(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        if database == SqliteDatabase::DurableCore {
            release = writing_release(&conn, database);
        }
        let Some((stamp, fleet)) = read(&conn, database)? else {
            return Ok(());
        };
        rows.push((database.name().to_owned(), stamp, fleet));
    }
    if rows
        .windows(2)
        .any(|pair| pair[0].1 != pair[1].1 || pair[0].2 != pair[1].2)
    {
        Err(incompatible(CompatRefusal::PartiallyAdvanced {
            databases: rows,
            writing_release: release,
        }))
    } else {
        Ok(())
    }
}

/// The handle answers the last `F` its writer fence observed, not the
/// open-time value (ADR 0115 §2.3).
impl lash_core_execution::FleetFormatStore for crate::SqliteStore {
    fn fleet_format(&self) -> FleetFormat {
        self.conn.fleet()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Column {
    name: String,
    ty: String,
    not_null: bool,
    default: Option<String>,
    primary_key: i64,
}

fn columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<Column>> {
    let mut statement =
        conn.prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1)")?;
    statement
        .query_map([table], |row| {
            Ok(Column {
                name: row.get(0)?,
                ty: row.get(1)?,
                not_null: row.get::<_, i64>(2)? != 0,
                default: row.get(3)?,
                primary_key: row.get(4)?,
            })
        })?
        .collect()
}

fn signature(conn: &Connection, table: &str, sql: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(sql)?;
    statement.query_map([table], |row| row.get(0))?.collect()
}

/// Expanded catalogs keep every required object and may add only write-safe
/// columns, tables, views and non-unique indexes.
pub(crate) fn verify_tolerant(conn: &Connection, database: SqliteDatabase) -> rusqlite::Result<()> {
    let baseline = Connection::open_in_memory()?;
    for statements in database.provisioning_statements() {
        baseline.execute_batch(statements)?;
    }
    let mut tables = baseline.prepare(
        "SELECT name, sql FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let expected: Vec<(String, String)> = tables
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut findings = Vec::new();
    for (table, expected_sql) in expected {
        let found_sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [&table],
                |row| row.get(0),
            )
            .optional()?;
        let Some(found_sql) = found_sql else {
            findings.push(format!("missing table {table}"));
            continue;
        };
        let expected_columns = columns(&baseline, &table)?;
        let found_columns = columns(conn, &table)?;
        for column in &expected_columns {
            if found_columns.iter().find(|found| found.name == column.name) != Some(column) {
                findings.push(format!("missing or changed column {table}.{}", column.name));
            }
        }
        for column in &found_columns {
            if !expected_columns
                .iter()
                .any(|expected| expected.name == column.name)
                && (column.not_null && column.default.is_none() || column.primary_key != 0)
            {
                findings.push(format!("required added column {table}.{}", column.name));
            }
        }
        let checks = |sql: &str| {
            sql.to_ascii_uppercase().matches("CHECK (").count()
                + sql.to_ascii_uppercase().matches("CHECK(").count()
        };
        if checks(&found_sql) > checks(&expected_sql) {
            findings.push(format!("added CHECK on {table}"));
        }
        for (description, sql) in [
            (
                "unique index",
                "SELECT name FROM pragma_index_list(?1) WHERE \"unique\" = 1",
            ),
            (
                "foreign key",
                "SELECT CAST(id AS TEXT) || ':' || \"table\" || ':' || \"from\" || ':' || \"to\" FROM pragma_foreign_key_list(?1)",
            ),
            (
                "trigger",
                "SELECT name FROM sqlite_schema WHERE type = 'trigger' AND tbl_name = ?1",
            ),
        ] {
            let baseline_items = signature(&baseline, &table, sql)?;
            let found_items = signature(conn, &table, sql)?;
            for item in found_items {
                if !baseline_items.contains(&item) {
                    findings.push(format!("added {description} {item} on {table}"));
                }
            }
            for item in baseline_items {
                if !signature(conn, &table, sql)?.contains(&item) {
                    findings.push(format!("missing {description} {item} on {table}"));
                }
            }
        }
    }
    if findings.is_empty() {
        Ok(())
    } else {
        Err(incompatible(CompatRefusal::ShapeRefused {
            component: database.component().as_str().to_owned(),
            findings,
            writing_release: writing_release(conn, database),
        }))
    }
}
