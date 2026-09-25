//! The fleet-format row of this PostgreSQL deployment (ADR 0106 §1 `F`).
//!
//! One `lash_fleet_format` singleton records the durable-format generation
//! every writer in the fleet emits. Unlike the single-process SQLite twin —
//! which finalizes the row on open — a shared store *provisions* it: the first
//! opener inserts this build's fleet format, every later open reads the
//! recorded generation, and only `lash admin finalize-upgrade` (FIG-3800) ever
//! moves it. That asymmetry is the rolling-deployment story: a newer worker
//! joining mid-upgrade finds the fleet's recorded value and keeps writing it.
//!
//! Durable writers never read the row directly — the open transaction reads it
//! once, the store handle carries it, and writers consult it through
//! [`FleetFormat::writer_version`]. An open that finds a recorded generation
//! outside this build's writable range is refused with a typed error rather
//! than allowed to emit a format the fleet has retired.

use lash_core_execution::{FleetFormat, FleetFormatState, StoreError};
use sqlx::{PgPool, Postgres, Transaction};

use crate::session_sql::session_sql;

/// Whether the role opening this store may write the fleet-format row.
///
/// A host-provisioned deployment can admit a role holding nothing but
/// `SELECT`, and that is a published property of that mode. The privilege is
/// asked for rather than discovered by letting the `INSERT` raise `42501` and
/// poison the admitting transaction — the same reason the release stamp asks.
async fn fleet_format_is_writable(tx: &mut Transaction<'_, Postgres>) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(session_sql().fleet_format.select_is_writable.sql())
        .fetch_one(&mut **tx)
        .await
}

/// Read the recorded fleet format inside a transaction.
///
/// `Ok(None)` is an absent row, not an absent table: callers run this only
/// where the schema gate has already admitted the table itself.
async fn read_in_tx(tx: &mut Transaction<'_, Postgres>) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar(session_sql().fleet_format.select_fleet_format.sql())
        .fetch_optional(&mut **tx)
        .await
}

/// The fleet format a recorded `format_version` names, checked against this
/// build's writable range.
///
/// Before the first format upgrade the range is one version wide, so any other
/// recorded value means the fleet has moved past — or never agreed with — this
/// build, and the open is refused with the typed error an operator can route.
fn recorded_fleet_format(version: i64) -> Result<FleetFormat, StoreError> {
    let Ok(version) = u32::try_from(version) else {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "lash_fleet_format.format_version",
            message: format!("not a fleet-format version: {version}"),
        });
    };
    if version != lash_core_execution::FLEET_FORMAT_VERSION {
        return Err(StoreError::FleetFormatOutsideWritableRange {
            recorded: version,
            current: lash_core_execution::FLEET_FORMAT_VERSION,
        });
    }
    Ok(FleetFormat::from_version(version))
}

/// Provision and read the fleet-format row inside the open transaction that
/// just admitted the schema.
///
/// A writable role inserts this build's fleet format when the row is absent
/// and leaves a recorded one alone — the "read both formats, write the fleet's
/// current one" arm of ADR 0106, where the first format's old and new are the
/// same value. A role that cannot write the row reads it instead, and an
/// entirely unrecorded store reports this build's own fleet format: the same
/// answer a recording store would give. So does a `SchemaCheck::WarnOnly`
/// open against a catalog that predates the table itself — the transaction is
/// aborted by the missing-relation error, which `commit` turns into a
/// rollback, matching [`read`]'s `Unrecorded` verdict.
pub(crate) async fn admit(tx: &mut Transaction<'_, Postgres>) -> Result<FleetFormat, StoreError> {
    if fleet_format_is_writable(tx)
        .await
        .map_err(crate::store_sqlx_error)?
    {
        sqlx::query(session_sql().fleet_format.insert_if_absent.sql())
            .bind(i32::try_from(lash_core_execution::FLEET_FORMAT_VERSION).unwrap_or(i32::MAX))
            .execute(&mut **tx)
            .await
            .map_err(crate::store_sqlx_error)?;
    }
    match read_in_tx(tx).await {
        Ok(Some(version)) => recorded_fleet_format(i64::from(version)),
        Ok(None) => Ok(FleetFormat::current()),
        Err(err) if missing_relation(&err) => Ok(FleetFormat::current()),
        Err(err) => Err(crate::store_sqlx_error(err)),
    }
}

/// The fleet format the store reports for inspection.
///
/// A database with no `lash_fleet_format` relation is
/// [`FleetFormatState::Unrecorded`]: it records no fleet format because no
/// build that writes one has opened it. A read that fails for any other
/// reason is [`FleetFormatState::Unreadable`] — an undecided row is not an
/// absent one.
pub(crate) async fn read(pool: &PgPool) -> FleetFormatState {
    let row: Result<Option<i32>, sqlx::Error> =
        sqlx::query_scalar(session_sql().fleet_format.select_fleet_format.sql())
            .fetch_optional(pool)
            .await;
    match row {
        Ok(Some(version)) => match u32::try_from(i64::from(version)) {
            Ok(version) => FleetFormatState::Recorded(FleetFormat::from_version(version)),
            Err(_) => FleetFormatState::Unreadable {
                reason: format!("lash_fleet_format.format_version is not a version: {version}"),
            },
        },
        Ok(None) => FleetFormatState::Unrecorded,
        Err(err) if missing_relation(&err) => FleetFormatState::Unrecorded,
        Err(err) => FleetFormatState::Unreadable {
            reason: err.to_string(),
        },
    }
}

/// `42P01 undefined_table` — the database predates the row rather than being
/// unreadable.
fn missing_relation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.code().map(|code| code.into_owned()))
        .is_some_and(|code| code == "42P01")
}
