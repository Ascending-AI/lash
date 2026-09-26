//! The fleet-format row of this SQLite deployment (ADR 0106 §1 `F`).
//!
//! One `fleet_format` singleton records the durable-format generation every
//! writer in the fleet emits. The row moves forward only: the schema-open
//! transaction provisions it when absent and leaves a recorded value alone —
//! the same insert-if-absent the PostgreSQL backend runs — and durable
//! writers consult the recorded value rather than a constant they baked in.
//! A build that opens a store recording a generation its writable range does
//! not admit is refused with a typed error instead of winding `F` back.
//!
//! The row lives in the durable-core database beside `release_stamp`: both are
//! facts about the store itself that every deployment carries exactly once.

use std::ops::RangeInclusive;

use lash_core_execution::{FleetFormat, FleetFormatState};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::session_sql::session_sql;

/// Whether the durable-core database carries the fleet-format table at all.
///
/// A pre-FLEET_FORMAT database does not, and that is an absence rather than a
/// read failure: it records no fleet format because no build that writes one
/// has opened it.
fn table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        crate::connection_sql::SELECT_FLEET_FORMAT_TABLE_EXISTS,
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Read the recorded fleet format.
///
/// An absent table or an empty one is [`FleetFormatState::Unrecorded`]: the
/// deployment predates the row or nothing has opened it yet. A row whose value
/// does not decode is [`FleetFormatState::Unreadable`] — an undecided row is
/// not an absent one.
pub(crate) fn read(conn: &Connection) -> rusqlite::Result<FleetFormatState> {
    if !table_exists(conn)? {
        return Ok(FleetFormatState::Unrecorded);
    }
    let row: Option<i64> = conn
        .query_row(
            session_sql().fleet_format.select_fleet_format.sql(),
            [],
            |row| row.get(0),
        )
        .optional()?;
    match row {
        Some(version) => match u32::try_from(version) {
            Ok(version) => Ok(FleetFormatState::Recorded(FleetFormat::from_version(
                version,
            ))),
            Err(_) => Ok(FleetFormatState::Unreadable {
                reason: format!("fleet_format.format_version is not a version: {version}"),
            }),
        },
        None => Ok(FleetFormatState::Unrecorded),
    }
}

/// Admit a recorded `version` under the writable range `writable` of the
/// build doing the opening.
///
/// The row is fail-closed (ADR 0106 §7): a generation the opening build
/// cannot write is refused with the typed
/// [`StoreError::FleetFormatOutsideWritableRange`] an operator can route,
/// carried inside the `rusqlite::Error` this layer returns so [`crate::sqlite_error`]
/// hands the typed variant back to callers that convert.
fn admit(version: u32, writable: RangeInclusive<u32>) -> rusqlite::Result<FleetFormat> {
    FleetFormat::admit_recorded(version, writable).map_err(crate::sqlite_conversion_error)
}

/// The fleet format the just-opened database records, admitted against this
/// build's writable range.
///
/// Called only after `prepare_versioned_schema` has provisioned or admitted
/// the durable-core schema, so the row is there: [`write`] ran in the same
/// transaction the open committed. Anything else is an internal defect, not a
/// deployment state.
///
/// `writable` is the opening build's `[min_F, max_F]` — production opens pass
/// [`FleetFormat::writable_range`], and a test simulating a different build
/// passes that build's range instead.
pub(crate) fn read_recorded(
    conn: &Connection,
    writable: RangeInclusive<u32>,
) -> rusqlite::Result<FleetFormat> {
    match read(conn)? {
        FleetFormatState::Recorded(format) => admit(format.version(), writable),
        state => Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERNAL),
            Some(format!(
                "fleet-format row {state} after the open transaction wrote it"
            )),
        )),
    }
}

/// Provision the fleet-format row inside the open transaction that just
/// provisioned or admitted the durable-core schema.
///
/// `F` moves forward only, so the write is insert-if-absent — the same arm
/// the shared PostgreSQL backend takes: an absent row records this build's
/// [`lash_core_execution::FLEET_FORMAT_VERSION`], and a recorded row wins.
/// When that constant moves, this write is the move.
pub(crate) fn write(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        session_sql().fleet_format.insert_if_absent.sql(),
        params![i64::from(lash_core_execution::FLEET_FORMAT_VERSION)],
    )?;
    Ok(())
}

/// The fleet format a read-only [`crate::Store`] reports.
///
/// A read-only open never runs the schema transaction, so a store only ever
/// opened by builds that predate the row reports the build's own fleet format
/// — the same answer a recording store would give — rather than failing an
/// open for a fact the handle cannot act on anyway. A recorded generation the
/// build's writable range does not admit is still refused: a reader that
/// cannot decode what the fleet writes is not a reader.
pub(crate) fn recorded_or_current(conn: &Connection) -> rusqlite::Result<FleetFormat> {
    match read(conn)? {
        FleetFormatState::Recorded(format) => {
            admit(format.version(), FleetFormat::writable_range())
        }
        _ => Ok(FleetFormat::current()),
    }
}

impl lash_core_execution::FleetFormatStore for crate::Store {
    /// The fleet format this store's durable writers emit — the `F` of ADR
    /// 0106 §1 as the fleet-format row recorded it at open.
    ///
    /// This is the hook durable writers consult for their writer version:
    /// `self.fleet_format().writer_version(surface_format!(…))` maps a
    /// format's build-newest version onto the generation the fleet agreed to
    /// write, which is the identity map until `finalize-upgrade` (FIG-3800)
    /// exists.
    fn fleet_format(&self) -> FleetFormat {
        self.fleet_format
    }
}
