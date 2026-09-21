//! Which lash release wrote this SQLite deployment.
//!
//! The four `PRAGMA user_version` integers say what *this* build requires. They
//! never say which build produced the rows, so the only way a host could learn
//! that was to upgrade crates, open the store, and read the refusal — which
//! names schema integers, not releases. The stamp closes that gap with one row
//! in the durable-core database: the writing release, the schema-version tuple
//! it required, and the instant that release first wrote here.
//!
//! **Update rule.** The stamp is written on the first open of an unstamped
//! store and advanced only by a strictly newer release
//! ([`release_stamp_advances`]). A reopen under the same release leaves the row
//! alone, so `written_at_epoch_ms` stays the instant this release took the
//! store over; an older build never downgrades the stamp; and a pair this build
//! cannot order leaves the existing row for an operator to read.
//!
//! The instant comes from the host clock rather than a runtime clock: the
//! schema-open path runs before any runtime exists, and the field is deployment
//! metadata rather than a durable domain fact that replay depends on.

use lash_core::{StoreComponentVersion, StoreReleaseStamp, StoreReleaseState};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::session_sql::session_sql;

use crate::schema::{
    EFFECT_SCHEMA_VERSION, PROCESS_SCHEMA_VERSION, SCHEMA_VERSION, SqliteDatabase,
    TRIGGER_SCHEMA_VERSION,
};

/// The release this build stamps into every store it writes.
///
/// `main` carries the honest `0.0.0-dev` placeholder in every manifest; the
/// release workflow stamps the real version into its ephemeral checkout before
/// building, so this constant *is* the release-time injection.
pub(crate) const BUILD_RELEASE: &str = env!("CARGO_PKG_VERSION");

/// What every SQLite component required when this build wrote the stamp.
///
/// All four constants live in one crate, so the durable-core row can record the
/// whole deployment's tuple even though only one database carries the stamp.
pub(crate) fn build_schema_versions() -> Vec<StoreComponentVersion> {
    [
        (SqliteDatabase::DurableCore, SCHEMA_VERSION),
        (SqliteDatabase::ProcessRegistry, PROCESS_SCHEMA_VERSION),
        (SqliteDatabase::Triggers, TRIGGER_SCHEMA_VERSION),
        (SqliteDatabase::EffectReplay, EFFECT_SCHEMA_VERSION),
    ]
    .into_iter()
    .map(|(database, version)| StoreComponentVersion {
        component: database.name().to_string(),
        version: i64::from(version),
    })
    .collect()
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Whether the durable-core database carries the stamp table at all.
///
/// A pre-stamp database does not, and that is an absence rather than a read
/// failure: it records no release because no build that stamps has written it.
pub(crate) fn table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        crate::connection_sql::SELECT_RELEASE_STAMP_TABLE_EXISTS,
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub(crate) fn read(conn: &Connection) -> rusqlite::Result<StoreReleaseState> {
    if !table_exists(conn)? {
        return Ok(StoreReleaseState::Unstamped);
    }
    let row: Option<(String, String, i64)> = conn
        .query_row(session_sql().release_stamp.select_stamp.sql(), [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    let Some((release, schema_versions, written_at_epoch_ms)) = row else {
        return Ok(StoreReleaseState::Unstamped);
    };
    let Some(schema_versions) = StoreReleaseStamp::decode_schema_versions(&schema_versions) else {
        return Ok(StoreReleaseState::Unreadable {
            reason: format!(
                "release_stamp.schema_versions is not a tuple this build can read: \
                 {schema_versions:?}"
            ),
        });
    };
    Ok(StoreReleaseState::Stamped(StoreReleaseStamp {
        release,
        schema_versions,
        written_at_epoch_ms,
    }))
}

/// The writing release alone, for a refusal message that has one to name.
///
/// Best effort by construction: the refusal it decorates is raised over a
/// database this build has already declined to read, so anything other than a
/// readable stamp yields `None` and the message simply says less.
pub(crate) fn read_release(conn: &Connection) -> Option<String> {
    match read(conn) {
        Ok(StoreReleaseState::Stamped(stamp)) => Some(stamp.release),
        // Anything else — an absent stamp, an unreadable one, a failed read, or
        // a state a later build adds — names no release, and a refusal that
        // cannot name one says less rather than guessing.
        Ok(_) | Err(_) => None,
    }
}

/// Apply the update rule inside the open transaction that just provisioned or
/// admitted the durable-core schema.
pub(crate) fn write(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    let existing: Option<String> = tx
        .query_row(
            session_sql().release_stamp.select_release.sql(),
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing
        && !lash_core::release_stamp_advances(&existing, BUILD_RELEASE)
    {
        return Ok(());
    }
    tx.execute(
        session_sql().release_stamp.upsert.sql(),
        params![
            BUILD_RELEASE,
            StoreReleaseStamp::encode_schema_versions(&build_schema_versions()),
            now_epoch_ms(),
        ],
    )?;
    Ok(())
}
