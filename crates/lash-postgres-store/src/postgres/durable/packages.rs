//! Package artifacts, sets and the artifacts each node holds (FIG-6918):
//! the PostgreSQL side of `lash_store_sql::durable::packages`.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_durable::packages::{
    ArtifactInstall, HeldArtifact, NodeCapability, PackageCatalog, SetInstall,
};
use lash_durable::{DurableError, DurableInstant};
use lash_store_sql::Dialect;
use lash_store_sql::durable::packages::PackageStatements;
use sqlx::PgConnection;

use super::sqlx_failure;

static SQL: LazyLock<PackageStatements> =
    LazyLock::new(|| PackageStatements::render(Dialect::postgres()));

lash_store_sql::statements! {
    /// The package statements only PostgreSQL issues.
    struct PostgresPackageStatements @ "durable_packages_postgres" {
        /// Serialize installs: a concurrent install would take the same
        /// next ordinal or install sequence. Readers are not blocked.
        lock = "LOCK TABLE package_sets, package_artifacts, package_labels
             IN SHARE ROW EXCLUSIVE MODE";
    }
}

static POSTGRES: LazyLock<PostgresPackageStatements> =
    LazyLock::new(|| PostgresPackageStatements::render(Dialect::postgres()));

/// Records that `node` holds each of `held`, in the registration's
/// transaction.
pub(super) async fn hold(
    tx: &mut PgConnection,
    node: &str,
    held: &BTreeSet<HeldArtifact>,
) -> Result<(), DurableError> {
    for artifact in held {
        crate::observed_sql::query(SQL.hold.sql())
            .bind(node)
            .bind(artifact.digest.to_string())
            .bind(artifact.path.as_str())
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    }
    Ok(())
}

/// Every node live at `now`, by node id.
pub(super) async fn live_nodes(
    tx: &mut PgConnection,
    now: DurableInstant,
) -> Result<Vec<NodeCapability>, DurableError> {
    let rows: Vec<lash_durable::packages::NodeRow> =
        crate::observed_sql::query_as(SQL.live_nodes.sql())
            .bind(now.0)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    lash_durable::packages::nodes_of(rows)
}

/// Installs `artifacts`, then `sets`, keeping what is stored. A set naming
/// an artifact the store does not hold is refused.
pub(super) async fn install(
    tx: &mut PgConnection,
    artifacts: &[ArtifactInstall],
    sets: &[SetInstall],
) -> Result<(), DurableError> {
    crate::observed_sql::query(POSTGRES.lock.sql())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    for artifact in artifacts {
        crate::observed_sql::query(SQL.artifact_put.sql())
            .bind(artifact.artifact.digest.to_string())
            .bind(artifact.artifact.path.as_str())
            .bind(artifact.header.as_str())
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        for function in &artifact.definitions {
            crate::observed_sql::query(SQL.definition_put.sql())
                .bind(function.to_string())
                .bind(artifact.artifact.path.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
        }
        for operation in &artifact.internal {
            crate::observed_sql::query(SQL.internal_put.sql())
                .bind(operation.as_str())
                .bind(artifact.artifact.path.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
        }
        crate::observed_sql::query(SQL.label_put.sql())
            .bind(artifact.artifact.digest.to_string())
            .bind(artifact.labels.as_str())
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    }
    for set in sets {
        for member in &set.members {
            let known = crate::observed_sql::query(SQL.artifact_known.sql())
                .bind(member.digest.to_string())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if known.is_none() {
                return Err(lash_durable::packages::unknown_member(set, member));
            }
        }
        let digest = set.digest.to_string();
        let installed = crate::observed_sql::query(SQL.set_put.sql())
            .bind(digest.as_str())
            .bind(set.surface.as_slice())
            .bind(lash_durable::packages::encode_requirements(
                &set.requirements,
            ))
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        if installed.is_some() {
            for member in &set.members {
                crate::observed_sql::query(SQL.member_put.sql())
                    .bind(digest.as_str())
                    .bind(member.digest.to_string())
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            }
        }
    }
    Ok(())
}

/// Every stored artifact and set.
pub(super) async fn catalog(tx: &mut PgConnection) -> Result<PackageCatalog, DurableError> {
    let artifacts: Vec<(String, String, i64)> = crate::observed_sql::query_as(SQL.artifacts.sql())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let members: Vec<(String, i64, String, String, String)> =
        crate::observed_sql::query_as(SQL.sets.sql())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    lash_durable::packages::catalog_of(artifacts, members)
}

/// One of the indexes installs write by key and package path.
#[derive(Clone, Copy)]
pub(super) enum Index {
    Definitions,
    InternalOperations,
}

/// The package paths `index` records for each of `keys`; a key it records
/// none for is absent.
pub(super) async fn paths<K: Ord + Clone + std::fmt::Display + Send + Sync>(
    tx: &mut PgConnection,
    index: Index,
    keys: &[K],
) -> Result<std::collections::BTreeMap<K, BTreeSet<String>>, DurableError> {
    let sql = match index {
        Index::Definitions => SQL.definition_paths.sql(),
        Index::InternalOperations => SQL.internal_paths.sql(),
    };
    let mut found = std::collections::BTreeMap::new();
    for key in keys {
        let paths: Vec<String> = crate::observed_sql::query_scalar(sql)
            .bind(key.to_string())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        if !paths.is_empty() {
            found.insert(key.clone(), paths.into_iter().collect());
        }
    }
    Ok(found)
}

/// Every labels each of `artifacts` was installed with, in install order.
pub(super) async fn labels(
    tx: &mut PgConnection,
    artifacts: &[lash_durable::ArtifactDigest],
) -> Result<std::collections::BTreeMap<lash_durable::ArtifactDigest, Vec<String>>, DurableError> {
    let mut found = std::collections::BTreeMap::new();
    for artifact in artifacts {
        let labels: Vec<String> = crate::observed_sql::query_scalar(SQL.labels_of.sql())
            .bind(artifact.to_string())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        if !labels.is_empty() {
            found.insert(*artifact, labels);
        }
    }
    Ok(found)
}

/// The surface snapshot stored for the set `digest`.
pub(super) async fn surface(
    tx: &mut PgConnection,
    digest: &str,
) -> Result<Option<Vec<u8>>, DurableError> {
    crate::observed_sql::query_scalar(SQL.surface.sql())
        .bind(digest)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)
}
