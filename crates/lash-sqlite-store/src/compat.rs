//! The compatibility row of the deployment's SQLite database.

use lash_core_execution::compat::{
    self, CompatAdmission, CompatRefusal, CompatStamp, StampRead, VersionRange,
};
#[cfg(any(test, feature = "testing"))]
use lash_core_execution::store::fleet_finalize::FleetEpochFlip;
use lash_core_execution::{FleetFormat, FleetFormatState, StoreError};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::location::SqliteLocation;
use crate::schema::COMPONENT;

fn incompatible(refusal: CompatRefusal) -> rusqlite::Error {
    crate::sqlite_conversion_error(StoreError::Incompatible { refusal })
}

pub(crate) fn writing_release(conn: &Connection) -> Option<String> {
    crate::release_stamp::read_release(conn)
}

/// A stamp refusal with the store's release evidence: the writing release,
/// and the reason as that release corrects it
/// ([`CompatRefusal::read_against_release`]).
fn attribute(refusal: CompatRefusal, release: Option<String>) -> CompatRefusal {
    refusal
        .read_against_release(release.as_deref(), crate::release_stamp::BUILD_RELEASE)
        .with_writing_release(release)
}

pub(crate) fn malformed(detail: impl Into<String>) -> rusqlite::Error {
    incompatible(CompatRefusal::MalformedStamp {
        component: COMPONENT.as_str().to_owned(),
        detail: detail.into(),
        writing_release: None,
    })
}

fn malformed_on(conn: &Connection, detail: impl Into<String>) -> rusqlite::Error {
    incompatible(CompatRefusal::MalformedStamp {
        component: COMPONENT.as_str().to_owned(),
        detail: detail.into(),
        writing_release: writing_release(conn),
    })
}

fn table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'lash_compat')",
        [],
        |row| row.get(0),
    )
}

/// Read the database's complete stamp. No row or table is absence, never a
/// defaulted current version.
pub(crate) fn read(conn: &Connection) -> rusqlite::Result<Option<(CompatStamp, u32)>> {
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
        .map_err(|error| malformed_on(conn, error.to_string()))?;
    let Some((component, version, min_reader, fleet_format)) = row else {
        return Ok(None);
    };
    if component != COMPONENT.as_str() {
        return Err(malformed_on(conn, format!("component is {component}")));
    }
    let version = u32::try_from(version).map_err(|error| malformed_on(conn, error.to_string()))?;
    let min_reader =
        u32::try_from(min_reader).map_err(|error| malformed_on(conn, error.to_string()))?;
    let fleet_format =
        u32::try_from(fleet_format).map_err(|error| malformed_on(conn, error.to_string()))?;
    Ok(Some((
        CompatStamp {
            version,
            min_reader,
        },
        fleet_format,
    )))
}

fn descriptor(conn: &Connection) -> rusqlite::Result<&'static compat::CompatDescriptor> {
    compat::descriptor(COMPONENT)
        .ok_or_else(|| malformed_on(conn, "the build has no descriptor for this database"))
}

pub(crate) fn admit(
    conn: &Connection,
    writable: VersionRange,
) -> rusqlite::Result<(CompatAdmission, FleetFormat)> {
    let row = read(conn)?;
    let stamp = row.map_or_else(
        || StampRead::Absent {
            populated: crate::schema::has_user_schema_objects(conn).unwrap_or(true),
        },
        |(stamp, _)| StampRead::Present(stamp),
    );
    let descriptor = descriptor(conn)?;
    let release = writing_release(conn);
    let admission = compat::admit(descriptor, stamp)
        .map_err(|refusal| incompatible(attribute(refusal, release.clone())))?;
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
pub(crate) fn fence(conn: &Connection, writable: VersionRange) -> rusqlite::Result<FleetFormat> {
    let row: Option<(String, i64, i64, i64)> = conn
        .prepare_cached(FENCE)
        .and_then(|mut statement| {
            statement
                .query_row([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .optional()
        })
        .map_err(|error| malformed_on(conn, error.to_string()))?;
    let Some((component, version, min_reader, fleet)) = row else {
        return Err(incompatible(CompatRefusal::Unstamped {
            component: COMPONENT.as_str().to_owned(),
            writing_release: writing_release(conn),
        }));
    };
    let descriptor = descriptor(conn)?;
    if component != COMPONENT.as_str() {
        return Err(malformed_on(conn, format!("component is {component}")));
    }
    let stamp = CompatStamp {
        version: u32::try_from(version).map_err(|error| malformed_on(conn, error.to_string()))?,
        min_reader: u32::try_from(min_reader)
            .map_err(|error| malformed_on(conn, error.to_string()))?,
    };
    compat::admit(descriptor, StampRead::Present(stamp))
        .map_err(|refusal| incompatible(attribute(refusal, writing_release(conn))))?;
    let fleet = u32::try_from(fleet).map_err(|error| malformed_on(conn, error.to_string()))?;
    FleetFormat::fence(fleet, writable).map_err(crate::sqlite_conversion_error)
}

/// The fleet record's per-plugin writer ranges (FIG-4746), which the
/// database carries beside `F`.
const PLUGIN_WRITERS: &str = "SELECT plugin_id, min_format, max_format FROM lash_plugin_writers";

const RECORD_PLUGIN_WRITER: &str =
    "INSERT INTO lash_plugin_writers (plugin_id, min_format, max_format)
     VALUES (?1, ?2, ?3)
     ON CONFLICT (plugin_id) DO UPDATE SET
         min_format = excluded.min_format,
         max_format = excluded.max_format";

fn plugin_writer_refusal(refusal: CompatRefusal) -> rusqlite::Error {
    incompatible(refusal)
}

/// Read the recorded writer ranges. A range that is not one, or a record
/// that cannot be read at all, is refused typed: the store fails closed.
pub(crate) fn read_plugin_writers(
    conn: &Connection,
) -> rusqlite::Result<lash_core_execution::store::plugin_writers::PluginWriterRanges> {
    let unreadable = |error: rusqlite::Error| {
        plugin_writer_refusal(CompatRefusal::PluginWriterRangeMalformed {
            plugin: String::new(),
            detail: format!("the writer ranges are unreadable: {error}"),
        })
    };
    let rows: Vec<(String, i64, i64)> = conn
        .prepare_cached(PLUGIN_WRITERS)
        .and_then(|mut statement| {
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect()
        })
        .map_err(unreadable)?;
    lash_core_execution::store::plugin_writers::PluginWriterRanges::from_rows(rows)
        .map_err(plugin_writer_refusal)
}

/// Record `entries` in the fleet record, replacing each plugin's range.
pub(crate) fn record_plugin_writers(
    conn: &Connection,
    entries: &std::collections::BTreeMap<String, VersionRange>,
) -> rusqlite::Result<()> {
    for (plugin, range) in entries {
        conn.prepare_cached(RECORD_PLUGIN_WRITER)?.execute(params![
            plugin,
            i64::from(range.min()),
            i64::from(range.max())
        ])?;
    }
    Ok(())
}

/// Admit `publication` against the recorded ranges inside the publishing
/// transaction, and record the entries it provisions.
pub(crate) fn admit_plugin_writers(
    tx: &Transaction<'_>,
    publication: &lash_core_execution::store::plugin_writers::PluginPublication,
) -> rusqlite::Result<()> {
    if publication.is_empty() {
        return Ok(());
    }
    let seeded = read_plugin_writers(tx)?
        .admit(publication)
        .map_err(plugin_writer_refusal)?;
    record_plugin_writers(tx, &seeded)
}

/// One step of [`advance_observed`], as its observer sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdvanceStep {
    /// `BEGIN EXCLUSIVE` holds the database.
    Locked,
    /// The rewrite committed.
    Committed,
}

/// Advance the store: a migration or an authorized finalize (ADR 0115 §2.2).
///
/// It takes `BEGIN EXCLUSIVE` on the database, lets `rewrite` change it (its
/// DDL and its `lash_compat` row) and commits. While it holds the database
/// no writer passes its fence, and a writer that was already past its fence
/// finishes first under the old row. The one transaction changes the whole
/// store or nothing. Reports the lock and the commit to `observe` as they
/// happen. An error from `observe` before the commit rolls the rewrite back,
/// as a crash at that point would.
pub(crate) fn advance_observed<T>(
    location: &SqliteLocation,
    busy_timeout: std::time::Duration,
    rewrite: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    mut observe: impl FnMut(AdvanceStep) -> rusqlite::Result<()>,
) -> rusqlite::Result<T> {
    let mut connection = Connection::open_with_flags(
        location.target().uri(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(busy_timeout)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)?;
    observe(AdvanceStep::Locked)?;
    let value = rewrite(&tx)?;
    tx.commit()?;
    observe(AdvanceStep::Committed)?;
    Ok(value)
}

/// A bare epoch flip for writer-fence tests. Production finalize checks a
/// retirement first through [`crate::finalize`].
#[cfg(any(test, feature = "testing"))]
pub(crate) fn flip_epoch_for_testing(
    location: &SqliteLocation,
    busy_timeout: std::time::Duration,
    writable: VersionRange,
) -> rusqlite::Result<FleetEpochFlip> {
    let target = writable.max();
    advance_observed(
        location,
        busy_timeout,
        |tx| {
            let recorded = fence(tx, writable)?.version();
            if recorded == target {
                return Ok(FleetEpochFlip::AlreadyFinalized { fleet: target });
            }
            tx.execute(
                "UPDATE lash_compat SET fleet_format = ?1 WHERE singleton = 1",
                [i64::from(target)],
            )?;
            Ok(FleetEpochFlip::Finalized {
                from: recorded,
                to: target,
            })
        },
        |_| Ok(()),
    )
}

/// Only the installer writes the row. An existing row is never changed by an
/// ordinary open, including an open by an older build after expand.
///
/// `F` starts at [`FleetFormat::seed`] of the provisioning build's writable
/// range `writable`, its floor: a compatibility release that provisions a
/// database leaves it inside the rollback window, and only finalize moves `F`
/// (ADR 0115 §2.1).
pub(crate) fn provision(tx: &Transaction<'_>, writable: VersionRange) -> rusqlite::Result<()> {
    let descriptor = descriptor(tx)?;
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
/// after a complete backup ([`crate::migration`]); a component's installer
/// never does.
pub(crate) fn refuse_unmigrated(conn: &Connection) -> rusqlite::Result<()> {
    let Some((stamp, _)) = read(conn)? else {
        return Ok(());
    };
    let target = crate::migration::target_version()?;
    if stamp.version < target {
        return Err(incompatible(CompatRefusal::MigrationPending {
            component: COMPONENT.as_str().to_owned(),
            found: stamp.version,
            target,
            writing_release: writing_release(conn),
        }));
    }
    Ok(())
}

pub(crate) fn read_recorded(
    conn: &Connection,
    writable: VersionRange,
) -> rusqlite::Result<FleetFormat> {
    let (_, fleet) = admit(conn, writable)?;
    Ok(fleet)
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn recorded_or_current(conn: &Connection) -> rusqlite::Result<FleetFormat> {
    read_recorded(conn, FleetFormat::writable())
}

pub(crate) fn read_fleet_state(conn: &Connection) -> rusqlite::Result<FleetFormatState> {
    match read(conn) {
        Ok(Some((_, fleet))) => Ok(FleetFormatState::Recorded(FleetFormat::from_version(fleet))),
        Ok(None) => Ok(FleetFormatState::Unrecorded),
        Err(error) => Ok(FleetFormatState::Unreadable {
            reason: error.to_string(),
        }),
    }
}

/// The handle answers the last `F` its writer fence observed, not the
/// open-time value (ADR 0115 §2.3).
impl lash_core_execution::FleetFormatStore for crate::SqliteStore {
    fn fleet_format(&self) -> FleetFormat {
        self.conn.fleet()
    }

    fn plugin_writers(&self) -> lash_core_execution::store::PluginWriterRangesFuture<'_> {
        Box::pin(async move {
            self.conn
                .read(|tx| read_plugin_writers(tx))
                .await
                .map_err(crate::sqlite_error)
        })
    }

    fn provision_plugin_writers<'a>(
        &'a self,
        registrations: &'a [lash_core_execution::store::plugin_writers::PluginWriterRegistration],
    ) -> lash_core_execution::store::PluginWriterRangesFuture<'a> {
        let registrations = registrations.to_vec();
        Box::pin(async move {
            self.conn
                .write(move |tx| {
                    let recorded = read_plugin_writers(tx)?;
                    let provisioned = recorded.provisioned(&registrations, tx.finalized());
                    record_plugin_writers(tx, &provisioned)?;
                    Ok(recorded.with(provisioned))
                })
                .await
                .map_err(crate::sqlite_error)
        })
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
pub(crate) fn verify_tolerant(conn: &Connection) -> rusqlite::Result<()> {
    let baseline = Connection::open_in_memory()?;
    for statements in crate::schema::provisioning_statements() {
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
            component: COMPONENT.as_str().to_owned(),
            findings,
            writing_release: writing_release(conn),
        }))
    }
}
