//! Which lash release wrote this PostgreSQL database.
//!
//! `lash_schema_versions` says what generation of the schema is on disk. It
//! never says which build produced it, so a host could only learn that by
//! upgrading crates, opening the store, and reading the refusal — and the
//! refusal names component integers, not releases. `lash_release_stamp` is the
//! missing fact: the writing release, the schema-version tuple it required, and
//! the instant that release first wrote here.
//!
//! **Update rule.** Written on the first open of an unstamped database and
//! advanced only by a strictly newer release
//! ([`lash_core_execution::release_stamp_advances`]). A reopen under the same release
//! leaves the row alone, so `written_at_epoch_ms` stays the instant this release
//! took the database over; an older build never downgrades the stamp; and a pair
//! this build cannot order leaves the existing row for an operator to read.
//!
//! The instant is the PostgreSQL server's, not the opening host's: two hosts
//! opening one database would otherwise stamp it from two unrelated clocks.

use lash_core_execution::{StoreComponentVersion, StoreReleaseStamp, StoreReleaseState};
use sqlx::{PgPool, Postgres, Transaction};

use crate::session_sql::session_sql;
use crate::{SCHEMA_COMPONENT, SCHEMA_VERSION};

/// The release this build stamps into every database it writes.
///
/// `main` carries the honest `0.0.0-dev` placeholder in every manifest; the
/// release workflow stamps the real version into its ephemeral checkout before
/// building, so this constant *is* the release-time injection.
pub(crate) const BUILD_RELEASE: &str = env!("CARGO_PKG_VERSION");

/// What this backend required when the stamp was written. PostgreSQL carries
/// one component, so the tuple has one entry.
pub(crate) fn build_schema_versions() -> Vec<StoreComponentVersion> {
    vec![StoreComponentVersion {
        component: SCHEMA_COMPONENT.to_string(),
        version: i64::from(SCHEMA_VERSION),
    }]
}

/// A reader that cannot write records no release and the store reports the
/// absence, which is the honest answer — it did not write these bytes. Why the
/// privilege is asked for rather than discovered is on the statement.
async fn stamp_is_writable(tx: &mut Transaction<'_, Postgres>) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(session_sql().release_stamp.select_is_writable.sql())
        .fetch_one(&mut **tx)
        .await
}

/// Apply the update rule inside the open transaction that just admitted the
/// schema.
///
/// `written_at_epoch_ms` is only recomputed when the row actually moves, so the
/// `RETURNING`-free upsert below is expressed as a conditional `DO UPDATE`
/// rather than a read-then-write: two openers racing the same first open
/// serialize on the primary key instead of both inserting.
pub(crate) async fn write(tx: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    if !stamp_is_writable(tx).await? {
        return Ok(());
    }
    let existing: Option<String> =
        sqlx::query_scalar(session_sql().release_stamp.select_release.sql())
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(existing) = existing
        && !lash_core_execution::release_stamp_advances(&existing, BUILD_RELEASE)
    {
        return Ok(());
    }
    sqlx::query(session_sql().release_stamp.upsert.sql())
        .bind(BUILD_RELEASE)
        .bind(StoreReleaseStamp::encode_schema_versions(
            &build_schema_versions(),
        ))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A database with no `lash_release_stamp` relation is
/// [`StoreReleaseState::Unstamped`]: it records no writing release because no
/// build that stamps has written it. A read that fails for any other reason is
/// [`StoreReleaseState::Unreadable`] — an undecided stamp is not an absent one.
pub(crate) async fn read(pool: &PgPool) -> StoreReleaseState {
    let row: Result<Option<(String, String, i64)>, sqlx::Error> =
        sqlx::query_as(session_sql().release_stamp.select_stamp.sql())
            .fetch_optional(pool)
            .await;
    match row {
        Ok(Some((release, encoded, written_at_epoch_ms))) => {
            let Some(schema_versions) = StoreReleaseStamp::decode_schema_versions(&encoded) else {
                return StoreReleaseState::Unreadable {
                    reason: format!(
                        "lash_release_stamp.schema_versions is not a tuple this build can read: \
                         {encoded:?}"
                    ),
                };
            };
            StoreReleaseState::Stamped(StoreReleaseStamp {
                release,
                schema_versions,
                written_at_epoch_ms,
            })
        }
        Ok(None) => StoreReleaseState::Unstamped,
        Err(err) if missing_relation(&err) => StoreReleaseState::Unstamped,
        Err(err) => StoreReleaseState::Unreadable {
            reason: err.to_string(),
        },
    }
}

/// The writing release alone, read inside the transaction that is about to
/// refuse the open.
///
/// Best effort by construction: the refusal it decorates is raised over a
/// database this build has already declined, so anything but a readable stamp
/// yields `None` and the message simply says less.
pub(crate) async fn read_release_in_tx(tx: &mut Transaction<'_, Postgres>) -> Option<String> {
    sqlx::query_scalar(session_sql().release_stamp.select_release.sql())
        .fetch_optional(&mut **tx)
        .await
        .ok()
        .flatten()
}

/// `42P01 undefined_table` — the database predates the stamp rather than being
/// unreadable.
fn missing_relation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.code().map(|code| code.into_owned()))
        .is_some_and(|code| code == "42P01")
}
