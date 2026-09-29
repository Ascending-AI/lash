use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_core_execution::{
    ArtifactReferrer, ArtifactStoreError, ReferrerClaim, ResolvedArtifactCleanup, StoreError,
};
use lash_store_sql::Dialect;
use lash_store_sql::artifact::referrer_edges::ReferrerEdgeStatements;
use lash_store_sql::artifact::referrer_fences::ReferrerFenceStatements;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};

use crate::*;

lash_store_sql::statements! {
    pub(crate) struct ReferrerPostgresStatements @ "artifact_referrer_edge" {
        delete_unreferenced = "DELETE FROM lashlang_artifacts AS artifact
             WHERE artifact.namespace = ?1 AND artifact.artifact_ref = ?2
               AND NOT EXISTS (SELECT 1 FROM artifact_referrer_edges AS edge
                   WHERE edge.namespace = artifact.namespace AND edge.artifact_ref = artifact.artifact_ref)";
    }
}

lash_store_sql::statements! {
    pub(crate) struct LashlangArtifactStatements @ "lashlang_artifact" {
        insert_bytes = "INSERT INTO lashlang_artifacts (namespace, artifact_ref, artifact_bytes)
             VALUES (?1, ?2, ?3) ON CONFLICT (namespace, artifact_ref) DO NOTHING";
        select_bytes = "SELECT artifact_bytes FROM lashlang_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2";
        exists = "SELECT EXISTS (SELECT 1 FROM lashlang_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2)";
        list_namespace_page = "SELECT artifact_ref, artifact_bytes FROM lashlang_artifacts
             WHERE namespace = ?1 AND (?2::text IS NULL OR artifact_ref > ?2::text)
             ORDER BY artifact_ref LIMIT ?3";
    }
}

pub(crate) struct ArtifactSql {
    pub(crate) edges: ReferrerEdgeStatements,
    pub(crate) fences: ReferrerFenceStatements,
    pub(crate) postgres: ReferrerPostgresStatements,
    pub(crate) lashlang_artifacts: LashlangArtifactStatements,
}

static ARTIFACT_SQL: LazyLock<ArtifactSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    ArtifactSql {
        edges: ReferrerEdgeStatements::render(dialect),
        fences: ReferrerFenceStatements::render(dialect),
        postgres: ReferrerPostgresStatements::render(dialect),
        lashlang_artifacts: LashlangArtifactStatements::render(dialect),
    }
});

pub(crate) fn artifact_sql() -> &'static ArtifactSql {
    &ARTIFACT_SQL
}

pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "lashlang_module";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";

fn backend(error: impl ToString) -> ArtifactStoreError {
    ArtifactStoreError::Backend(error.to_string())
}

pub(crate) async fn lock_referrer_tx(
    conn: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<(), sqlx::Error> {
    let key = format!(
        "lash-artifact-referrer:{}:{}",
        referrer.kind().as_str(),
        referrer.canonical_id()
    );
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(key)
    .execute(conn)
    .await
    .map(|_| ())
}

async fn lock_artifact_tx(
    tx: &mut Transaction<'_, Postgres>,
    namespace: &str,
    artifact_ref: &str,
) -> Result<(), ArtifactStoreError> {
    let key = format!("lash-artifact:{namespace}:{artifact_ref}");
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(key)
    .execute(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(())
}

async fn is_fenced_tx(
    tx: &mut Transaction<'_, Postgres>,
    referrer: &ArtifactReferrer,
) -> Result<bool, ArtifactStoreError> {
    sqlx::query_scalar(artifact_sql().fences.select_is_fenced.sql())
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .fetch_one(&mut **tx)
        .await
        .map_err(backend)
}

fn decode_edge_referrer(row: &PgRow) -> Result<ArtifactReferrer, ArtifactStoreError> {
    let kind: String = row.try_get("referrer_kind").map_err(backend)?;
    let id: String = row.try_get("referrer_id").map_err(backend)?;
    ArtifactReferrer::decode(&kind, &id).map_err(|error| {
        StoreError::StoredDataCorrupt {
            record_kind: "artifact_referrer_edge",
            message: error.to_string(),
        }
        .into()
    })
}

impl PostgresLashlangArtifactStore {
    async fn write_namespaced(
        &self,
        namespace: &str,
        artifact_ref: &str,
        bytes: Option<&[u8]>,
        claim: &ReferrerClaim,
    ) -> Result<(), ArtifactStoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_referrer_tx(&mut tx, claim.referrer())
            .await
            .map_err(backend)?;
        if is_fenced_tx(&mut tx, claim.referrer()).await? {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: claim.referrer().clone(),
            });
        }
        lock_artifact_tx(&mut tx, namespace, artifact_ref).await?;
        if let Some(cleanup) = claim.guard_cleanup() {
            let now = crate::support::postgres_transaction_epoch_ms(&mut tx)
                .await
                .map_err(ArtifactStoreError::from)?;
            crate::obligation_ledger::arm_cleanup_tx(&mut tx, &cleanup, now)
                .await
                .map_err(ArtifactStoreError::from)?;
        }
        if let Some(bytes) = bytes {
            sqlx::query(artifact_sql().lashlang_artifacts.insert_bytes.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .bind(bytes)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
            let stored: Vec<u8> =
                sqlx::query_scalar(artifact_sql().lashlang_artifacts.select_bytes.sql())
                    .bind(namespace)
                    .bind(artifact_ref)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(backend)?;
            if stored != bytes {
                return Err(ArtifactStoreError::Immutable {
                    artifact_ref: artifact_ref.to_owned(),
                });
            }
        } else {
            let exists: bool = sqlx::query_scalar(artifact_sql().lashlang_artifacts.exists.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .fetch_one(&mut *tx)
                .await
                .map_err(backend)?;
            if !exists {
                return Err(ArtifactStoreError::ArtifactMissing {
                    artifact_ref: artifact_ref.to_owned(),
                });
            }
        }
        sqlx::query(artifact_sql().edges.insert_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(claim.referrer().kind().as_str())
            .bind(claim.referrer().canonical_id())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    async fn end_namespaced(
        &self,
        namespace: &str,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let mut referrers: Vec<ArtifactReferrer> = cleanup
            .carries
            .iter()
            .map(|carry| carry.to.clone())
            .collect();
        referrers.push(cleanup.referrer.clone());
        referrers.sort_by_key(|referrer| {
            format!(
                "lash-artifact-referrer:{}:{}",
                referrer.kind().as_str(),
                referrer.canonical_id()
            )
        });
        referrers.dedup();
        for referrer in &referrers {
            lock_referrer_tx(&mut tx, referrer).await.map_err(backend)?;
        }
        let edges = sqlx::query(
            artifact_sql()
                .edges
                .select_referrer_edges_in_namespace
                .sql(),
        )
        .bind(namespace)
        .bind(cleanup.referrer.kind().as_str())
        .bind(cleanup.referrer.canonical_id())
        .fetch_all(&mut *tx)
        .await
        .map_err(backend)?;
        let source_refs: BTreeSet<String> = edges
            .iter()
            .map(|row| {
                decode_edge_referrer(row)?;
                row.try_get::<String, _>(1).map_err(backend)
            })
            .collect::<Result<_, _>>()?;
        let all_refs: BTreeSet<String> = source_refs
            .iter()
            .cloned()
            .chain(
                cleanup
                    .carries
                    .iter()
                    .map(|carry| carry.artifact.artifact_ref.clone()),
            )
            .collect();
        for artifact_ref in &all_refs {
            lock_artifact_tx(&mut tx, namespace, artifact_ref).await?;
        }
        let now = crate::support::postgres_transaction_epoch_ms(&mut tx)
            .await
            .map_err(ArtifactStoreError::from)?;
        sqlx::query(artifact_sql().fences.insert_fence.sql())
            .bind(cleanup.referrer.kind().as_str())
            .bind(cleanup.referrer.canonical_id())
            .bind(crate::support::clamp_epoch_ms(now))
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        for carry in &cleanup.carries {
            if is_fenced_tx(&mut tx, &carry.to).await? {
                continue;
            }
            let artifact_ref = &carry.artifact.artifact_ref;
            let exists: bool = sqlx::query_scalar(artifact_sql().lashlang_artifacts.exists.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .fetch_one(&mut *tx)
                .await
                .map_err(backend)?;
            if !exists {
                return Err(ArtifactStoreError::CarryArtifactMissing {
                    artifact_ref: artifact_ref.clone(),
                    to: carry.to.clone(),
                });
            }
            sqlx::query(artifact_sql().edges.insert_edge.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .bind(carry.to.kind().as_str())
                .bind(carry.to.canonical_id())
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
        }
        sqlx::query(
            artifact_sql()
                .edges
                .delete_referrer_edges_in_namespace
                .sql(),
        )
        .bind(namespace)
        .bind(cleanup.referrer.kind().as_str())
        .bind(cleanup.referrer.canonical_id())
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        for artifact_ref in &source_refs {
            sqlx::query(artifact_sql().postgres.delete_unreferenced.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)
    }

    async fn get_namespaced(
        &self,
        namespace: &str,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        let edges = sqlx::query(artifact_sql().edges.select_artifact_edges.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        for row in &edges {
            decode_edge_referrer(row)?;
        }
        sqlx::query_scalar(artifact_sql().lashlang_artifacts.select_bytes.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ModuleArtifactStore for PostgresLashlangArtifactStore {
    fn pause_next_publication_for_testing(
        &self,
    ) -> Option<lash_core_execution::ArtifactPublicationPause> {
        let pause = lash_core_execution::ArtifactPublicationPause::default();
        *self
            .publication_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pause.clone());
        Some(pause)
    }

    fn durability_tier(&self) -> lash_core_execution::DurabilityTier {
        lash_core_execution::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        let pause = self
            .publication_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pause) = pause {
            pause.pause().await;
        }
        self.write_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref, Some(bytes), claim)
            .await
    }

    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.write_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref, None, claim)
            .await
    }

    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_namespaced(MODULE_ARTIFACT_NAMESPACE, cleanup)
            .await
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.get_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessExecutionEnvStore for PostgresLashlangArtifactStore {
    async fn publish_process_execution_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        if !env_ref.matches_store_bytes(bytes) {
            return Err(ArtifactStoreError::Immutable {
                artifact_ref: env_ref.as_str().to_owned(),
            });
        }
        self.write_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str(), Some(bytes), claim)
            .await
    }

    async fn acquire_process_execution_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        self.write_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str(), None, claim)
            .await
    }

    async fn end_process_env_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_namespaced(PROCESS_ENV_NAMESPACE, cleanup).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        self.get_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str())
            .await
    }
}
