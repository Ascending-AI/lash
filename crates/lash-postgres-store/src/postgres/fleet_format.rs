//! The fleet-format row of this PostgreSQL deployment (ADR 0106 §1 `F`).
//!
//! One `lash_fleet_format` singleton records the durable-format generation
//! every writer in the fleet emits. The installer seeds it — `lashctl migrate`
//! at the migrating build's [`FleetFormat::seed`], or a host applying
//! `schema.sql` — every open reads the recorded generation, and only
//! `lash admin finalize-upgrade` (FIG-3800) ever moves it. An open never
//! records `F`: were the first opener to decide it, an N+1 that opened a
//! freshly migrated store before any N would record its own epoch and skip the
//! rollback window (ADR 0115 §2.1).
//!
//! Durable writers never read the row directly — the open transaction reads it
//! once, the store handle carries it, and writers consult it through
//! [`FleetFormat::writer_version`]. An open that finds no recorded generation,
//! or one outside this build's writable range, is refused with a typed error
//! rather than allowed to emit a format the fleet never agreed to.

use lash_core_execution::compat::VersionRange;

use lash_core_execution::{FleetFormat, FleetFormatState, StoreError};
use sqlx::{PgPool, Postgres, Transaction};

use crate::session_sql::session_sql;

/// The component a missing fleet epoch is reported against.
const COMPONENT: &str = "postgres";

/// Read the recorded fleet format inside a transaction.
///
/// `Ok(None)` is an absent row, not an absent table: callers run this only
/// where the schema gate has already admitted the table itself.
async fn read_in_tx(tx: &mut Transaction<'_, Postgres>) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar(session_sql().fleet_format.select_fleet_format.sql())
        .fetch_optional(&mut **tx)
        .await
}

/// The fleet format a recorded `format_version` names, admitted against the
/// writable range `writable` of the build doing the opening.
fn recorded_fleet_format(version: i32, writable: VersionRange) -> Result<FleetFormat, StoreError> {
    let Ok(version) = u32::try_from(version) else {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "lash_fleet_format.format_version",
            message: format!("not a fleet-format version: {version}"),
        });
    };
    FleetFormat::admit_recorded(COMPONENT, Some(version), writable)
}

/// The refusal an open answers when the store records no fleet epoch.
pub(crate) fn unrecorded(writable: VersionRange) -> Result<FleetFormat, StoreError> {
    FleetFormat::admit_recorded(COMPONENT, None, writable)
}

/// Read and admit the fleet-format row inside the open transaction that just
/// admitted the schema.
///
/// The open only reads, so a role holding nothing but `SELECT` opens exactly
/// as a writing role does. A store with no row — migrated by a build that did
/// not seed it, or a `SchemaCheck::WarnOnly` open against a catalog without
/// the table — refuses with [`CompatRefusal::FleetUnrecorded`] and records
/// nothing; `lashctl migrate` seeds it. `writable` is the opening build's
/// `[min_F, max_F]` — production opens pass [`FleetFormat::writable`], and a
/// test simulating a different build passes that build's range instead.
///
/// [`CompatRefusal::FleetUnrecorded`]: lash_core_execution::compat::CompatRefusal::FleetUnrecorded
pub(crate) async fn admit(
    tx: &mut Transaction<'_, Postgres>,
    writable: VersionRange,
) -> Result<FleetFormat, StoreError> {
    match read_in_tx(tx).await {
        Ok(Some(version)) => recorded_fleet_format(version, writable),
        Ok(None) => unrecorded(writable),
        Err(err) if missing_relation(&err) => unrecorded(writable),
        Err(err) => Err(crate::store_sqlx_error(err)),
    }
}

/// Seed the fleet-format row at `fleet` when the store records none: the
/// installer's half of `F` (ADR 0115 §2.1), run by `lash migrate` under its
/// exclusive advisory lock. A recorded row is left alone — moving it is
/// finalize's job — so a rerun, or a newer build's migrate, never changes it.
pub(crate) async fn seed(
    tx: &mut Transaction<'_, Postgres>,
    fleet: FleetFormat,
) -> Result<(), StoreError> {
    let version = i32::try_from(fleet.version()).map_err(|_| StoreError::StoredDataCorrupt {
        record_kind: "lash_fleet_format.format_version",
        message: format!("not a fleet-format version: {fleet}"),
    })?;
    sqlx::query(session_sql().fleet_format.insert_if_absent.sql())
        .bind(version)
        .execute(&mut **tx)
        .await
        .map_err(crate::store_sqlx_error)?;
    Ok(())
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

impl lash_core_execution::FleetFormatStore for crate::PostgresStore {
    /// The fleet format this store's durable writers emit — the `F` of ADR
    /// 0106 §1 the opening `PostgresStorage` admitted.
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
