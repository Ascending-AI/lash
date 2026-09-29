//! Which lash release wrote this PostgreSQL database.
//!
//! `lash_schema_versions` says what generation of the schema is on disk. It
//! never says which build produced it, so a host could only learn that by
//! upgrading crates, opening the store, and reading the refusal. The component
//! integer alone cannot identify that build. `lash_release_stamp` records the
//! writing release, the schema-version tuple it required, and
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
use sqlx::{Postgres, Transaction};

use crate::SCHEMA_COMPONENT;
use crate::session_sql::session_sql;

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
        version: 1,
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
async fn read<'e, E>(executor: E) -> StoreReleaseState
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let row: Result<Option<(String, String, i64)>, sqlx::Error> =
        sqlx::query_as(session_sql().release_stamp.select_stamp.sql())
            .fetch_optional(executor)
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

/// Keep optional relation errors local to this probe, preserving the caller's
/// snapshot for fleet and compatibility facts even when the query fails.
pub(crate) async fn read_state_in_tx(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<StoreReleaseState, sqlx::Error> {
    let mut probe = sqlx::Acquire::begin(tx).await?;
    let state = read(&mut *probe).await;
    probe.rollback().await?;
    Ok(state)
}

/// The writing release alone, read inside the verifying open transaction.
///
/// Best effort by construction: anything but a readable stamp yields `None`,
/// and a later refusal says less. A failed read rolls back to a savepoint so
/// the compatibility gate can still inspect the database.
pub(crate) async fn read_release_in_tx(tx: &mut Transaction<'_, Postgres>) -> Option<String> {
    // A missing or unreadable release table must not poison the open
    // transaction that is about to report a different compatibility refusal.
    sqlx::query("SAVEPOINT lash_release_read")
        .execute(&mut **tx)
        .await
        .ok()?;
    let release = sqlx::query_scalar(session_sql().release_stamp.select_release.sql())
        .fetch_optional(&mut **tx)
        .await;
    if release.is_err() {
        sqlx::query("ROLLBACK TO SAVEPOINT lash_release_read")
            .execute(&mut **tx)
            .await
            .ok()?;
    }
    sqlx::query("RELEASE SAVEPOINT lash_release_read")
        .execute(&mut **tx)
        .await
        .ok()?;
    release.ok().flatten()
}

/// `42P01 undefined_table` — the database predates the stamp rather than being
/// unreadable.
fn missing_relation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.code().map(|code| code.into_owned()))
        .is_some_and(|code| code == "42P01")
}
