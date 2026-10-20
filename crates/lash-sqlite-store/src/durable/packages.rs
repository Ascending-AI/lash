//! Package artifacts, sets and the artifacts each node holds (FIG-6918):
//! the SQLite side of `lash_store_sql::durable::packages`.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_durable::packages::{
    ArtifactInstall, HeldArtifact, NodeCapability, PackageCatalog, SetInstall,
};
use lash_durable::{DurableError, DurableInstant};
use lash_store_sql::durable::packages::PackageStatements;
use rusqlite::{Connection, OptionalExtension};

use crate::conn::cached_execute;

static SQL: LazyLock<PackageStatements> =
    LazyLock::new(|| PackageStatements::render(crate::schema_layout::MAIN));

/// Records that `node` holds each of `held`, in the registration's
/// transaction.
pub(super) fn hold(
    tx: &Connection,
    node: &str,
    held: &BTreeSet<HeldArtifact>,
) -> rusqlite::Result<()> {
    for artifact in held {
        cached_execute(
            tx,
            SQL.hold.sql(),
            rusqlite::params![node, artifact.digest.to_string(), artifact.path],
        )?;
    }
    Ok(())
}

/// Every node live at `now`, by node id.
pub(super) fn live_nodes(
    tx: &Connection,
    now: DurableInstant,
) -> rusqlite::Result<Result<Vec<NodeCapability>, DurableError>> {
    let rows = tx
        .prepare_cached(SQL.live_nodes.sql())?
        .query_map([now.0], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(lash_durable::packages::nodes_of(rows))
}

/// Installs `artifacts`, then `sets`, keeping what is stored. A set naming
/// an artifact the store does not hold is refused.
pub(super) fn install(
    tx: &Connection,
    artifacts: &[ArtifactInstall],
    sets: &[SetInstall],
) -> rusqlite::Result<Result<(), DurableError>> {
    for artifact in artifacts {
        cached_execute(
            tx,
            SQL.artifact_put.sql(),
            rusqlite::params![
                artifact.artifact.digest.to_string(),
                artifact.artifact.path,
                artifact.header
            ],
        )?;
    }
    for set in sets {
        for member in &set.members {
            let known = tx
                .prepare_cached(SQL.artifact_known.sql())?
                .query_row([member.digest.to_string()], |_| Ok(()))
                .optional()?;
            if known.is_none() {
                return Ok(Err(lash_durable::packages::unknown_member(set, member)));
            }
        }
        let digest = set.digest.to_string();
        let installed = tx
            .prepare_cached(SQL.set_put.sql())?
            .query_row([digest.as_str()], |_| Ok(()))
            .optional()?;
        if installed.is_some() {
            for member in &set.members {
                cached_execute(
                    tx,
                    SQL.member_put.sql(),
                    rusqlite::params![digest, member.digest.to_string()],
                )?;
            }
        }
    }
    Ok(Ok(()))
}

/// Every stored artifact and set.
pub(super) fn catalog(tx: &Connection) -> rusqlite::Result<Result<PackageCatalog, DurableError>> {
    let artifacts = tx
        .prepare_cached(SQL.artifacts.sql())?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let members = tx
        .prepare_cached(SQL.sets.sql())?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(lash_durable::packages::catalog_of(artifacts, members))
}
