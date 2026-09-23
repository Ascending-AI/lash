use std::sync::LazyLock;

use lash_store_sql::Dialect;
use lash_store_sql::artifact::owner_retirements::OwnerRetirementStatements;
use lash_store_sql::artifact::owners::OwnerStatements;

use crate::*;

lash_store_sql::statements! {
    /// `lash_artifact_owners` statements only PostgreSQL issues.
    pub(crate) struct OwnerPostgresStatements @ "artifact_owner" {
        /// Only PostgreSQL has them: SQLite reaches its bytes through the `artifact_refs`
        /// pointer table and reclaims a blob instead.
        delete_unowned_artifact = "DELETE FROM lashlang_artifacts AS artifact
             WHERE artifact.namespace = ?1 AND artifact.artifact_ref = ?2
               AND NOT EXISTS (
                   SELECT 1 FROM artifact_owners AS owner
                   WHERE owner.namespace = artifact.namespace
                     AND owner.artifact_ref = artifact.artifact_ref
               )";

        /// The same reclaim over every reference in `?2`, issued once after a
        /// retirement severs an owner's whole edge set.
        delete_unowned_artifacts_for_owner = "DELETE FROM lashlang_artifacts AS artifact
             WHERE artifact.namespace = ?1
               AND artifact.artifact_ref = ANY(?2)
               AND NOT EXISTS (
                   SELECT 1 FROM artifact_owners AS owner
                   WHERE owner.namespace = artifact.namespace
                     AND owner.artifact_ref = artifact.artifact_ref
               )";

        /// Every artifact owner `?2`/`?3` holds in namespace `?1`, in a
        /// stable order.
        ///
        /// The ordering is the fork, and it is load-bearing here: this
        /// backend takes a per-artifact advisory lock for each row it reads,
        /// so the read must hand them over in one order for every caller.
        /// SQLite reclaims each reference under the single write lock it
        /// already holds and has no lock order to keep.
        select_owned_refs = "SELECT artifact_ref FROM artifact_owners
             WHERE namespace = ?1 AND owner_kind = ?2 AND owner_id = ?3
             ORDER BY artifact_ref";
    }
}

lash_store_sql::statements! {
    /// `lash_lashlang_artifacts` statements. The table has no SQLite half —
    /// SQLite reaches the same bytes through `blobs` and `artifact_refs` — so
    /// every statement over it is PostgreSQL-only by construction.
    pub(crate) struct LashlangArtifactStatements @ "lashlang_artifact" {
        /// Publish `?3` as artifact `?1`/`?2`. Publishing the same reference
        /// twice is the same fact as publishing it once; the bytes are
        /// content-addressed, so a conflict is the same bytes.
        insert_bytes = "INSERT INTO lashlang_artifacts (namespace, artifact_ref, artifact_bytes)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (namespace, artifact_ref)
             DO NOTHING";

        /// Artifact `?1`/`?2`'s bytes.
        ///
        /// One name for one text: the publish path reads it back to prove
        /// immutability and the get path reads it to serve a caller, and
        /// before FIG-3387 those were two verbatim copies of this statement
        /// in one module.
        select_bytes = "SELECT artifact_bytes FROM lashlang_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2";

        exists = "SELECT EXISTS (
                 SELECT 1 FROM lashlang_artifacts
                 WHERE namespace = ?1 AND artifact_ref = ?2
             )";

        /// One page of namespace `?1`'s artifacts after `?2`, at most `?3`
        /// rows, ordered by the content-addressed reference so a preflight
        /// walk resumes without a table-sized offset scan.
        list_namespace_page = "SELECT artifact_ref, artifact_bytes
             FROM lashlang_artifacts
             WHERE namespace = ?1
               AND (?2::text IS NULL OR artifact_ref > ?2::text)
             ORDER BY artifact_ref
             LIMIT ?3";
    }
}

/// Every artifact-owner statement, rendered once.
pub(crate) struct ArtifactSql {
    /// `artifact_owners` statements both backends issue verbatim.
    pub(crate) owners: OwnerStatements,
    /// `artifact_owners` statements only PostgreSQL issues.
    pub(crate) owners_postgres: OwnerPostgresStatements,
    /// `artifact_owner_retirements` statements both backends issue verbatim.
    pub(crate) retirements: OwnerRetirementStatements,
    /// `lashlang_artifacts` statements, all of them PostgreSQL-only.
    pub(crate) lashlang_artifacts: LashlangArtifactStatements,
}

static ARTIFACT_SQL: LazyLock<ArtifactSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    ArtifactSql {
        owners: OwnerStatements::render(dialect),
        owners_postgres: OwnerPostgresStatements::render(dialect),
        retirements: OwnerRetirementStatements::render(dialect),
        lashlang_artifacts: LashlangArtifactStatements::render(dialect),
    }
});

/// The artifact-owner statements, rendered once at first use.
pub(crate) fn artifact_sql() -> &'static ArtifactSql {
    &ARTIFACT_SQL
}

/// Logical keyspaces multiplexed onto `lash_lashlang_artifacts`.
pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "lashlang_module";
const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";

/// A refusal raised inside the shared artifact-owner helpers, kept typed so
/// both the lashlang and the process-execution-env boundaries classify it by
/// variant rather than by message text.
enum ArtifactStoreFailure {
    /// The write named an owner a permanent retirement fence has closed.
    OwnerRetired,
    /// A transfer named a destination owner already fenced by retirement.
    DestinationOwnerRetired,
    /// A transfer found neither the staging owner's edge nor the
    /// destination's.
    StagingEdgeMissing { artifact: String },
    /// Any other backend failure; the message is the whole diagnostic.
    Backend(String),
}

impl ArtifactStoreFailure {
    fn into_plugin_error(self) -> lash_core_execution::PluginError {
        match self {
            Self::OwnerRetired => {
                lash_core_execution::runtime::process::artifact_owner_retired_error()
            }
            Self::DestinationOwnerRetired => {
                lash_core_execution::runtime::process::artifact_destination_owner_retired_error()
            }
            Self::StagingEdgeMissing { artifact } => {
                lash_core_execution::runtime::process::artifact_staging_edge_missing_error(artifact)
            }
            Self::Backend(message) => lash_core_execution::PluginError::Session(message),
        }
    }

    #[cfg(feature = "lashlang")]
    fn into_artifact_store_error(self) -> lashlang::ArtifactStoreError {
        match self {
            Self::OwnerRetired => lashlang::ArtifactStoreError::OwnerRetired,
            Self::DestinationOwnerRetired => lashlang::ArtifactStoreError::DestinationOwnerRetired,
            Self::StagingEdgeMissing { artifact } => {
                lashlang::ArtifactStoreError::StagingEdgeMissing { artifact }
            }
            Self::Backend(message) => lashlang::ArtifactStoreError::Backend(message),
        }
    }
}

impl PostgresLashlangArtifactStore {
    async fn lock_owner(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        owner_kind: &str,
        owner_id: &str,
    ) -> Result<(), sqlx::Error> {
        let key = format!("lash-artifact-owner:{owner_kind}:{owner_id}");
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(key)
        .execute(&mut **tx)
        .await
        .map(|_| ())
    }

    /// Serialize every mutation of one logical artifact at a stable PostgreSQL
    /// advisory-lock key. Row locks are insufficient because both the bytes row
    /// and its final owner edge may legitimately disappear during release.
    async fn lock_artifact(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        namespace: &str,
        artifact_ref: &str,
    ) -> Result<(), sqlx::Error> {
        let key = format!("lash-artifact:{namespace}:{artifact_ref}");
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(key)
        .execute(&mut **tx)
        .await
        .map(|_| ())
    }

    async fn publish_namespaced_bytes(
        &self,
        namespace: &str,
        artifact_ref: &str,
        bytes: &[u8],
        owner: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), ArtifactStoreFailure> {
        let (owner_kind, owner_id) = owner
            .storage_parts()
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        Self::lock_owner(&mut tx, owner_kind, &owner_id)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        Self::lock_artifact(&mut tx, namespace, artifact_ref)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let retired: bool = sqlx::query_scalar(artifact_sql().retirements.select_is_retired.sql())
            .bind(owner_kind)
            .bind(&owner_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        if retired {
            return Err(ArtifactStoreFailure::OwnerRetired);
        }
        sqlx::query(artifact_sql().lashlang_artifacts.insert_bytes.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(bytes)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let stored: Vec<u8> =
            sqlx::query_scalar(artifact_sql().lashlang_artifacts.select_bytes.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        if stored != bytes {
            return Err(ArtifactStoreFailure::Backend(format!(
                "artifact `{artifact_ref}` in namespace `{namespace}` is immutable"
            )));
        }
        sqlx::query(artifact_sql().owners.insert_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(owner_kind)
            .bind(owner_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))
    }

    async fn get_namespaced_bytes(
        &self,
        namespace: &str,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, sqlx::Error> {
        sqlx::query_scalar(artifact_sql().lashlang_artifacts.select_bytes.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .fetch_optional(&self.pool)
            .await
    }

    async fn retain_namespaced_bytes(
        &self,
        namespace: &str,
        artifact_ref: &str,
        owner: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), ArtifactStoreFailure> {
        let (owner_kind, owner_id) = owner
            .storage_parts()
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        Self::lock_owner(&mut tx, owner_kind, &owner_id)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        Self::lock_artifact(&mut tx, namespace, artifact_ref)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let retired: bool = sqlx::query_scalar(artifact_sql().retirements.select_is_retired.sql())
            .bind(owner_kind)
            .bind(&owner_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        if retired {
            return Err(ArtifactStoreFailure::OwnerRetired);
        }
        let exists: bool = sqlx::query_scalar(artifact_sql().lashlang_artifacts.exists.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        if !exists {
            return Err(ArtifactStoreFailure::Backend(format!(
                "missing artifact `{artifact_ref}`"
            )));
        }
        sqlx::query(artifact_sql().owners.insert_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(owner_kind)
            .bind(owner_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))
    }

    async fn transfer_namespaced_owner(
        &self,
        namespace: &str,
        artifact_ref: &str,
        from: &lash_core_execution::ArtifactOwner,
        to: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), ArtifactStoreFailure> {
        let (from_kind, from_id) = from
            .storage_parts()
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let (to_kind, to_id) = to
            .storage_parts()
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let mut locks = [(from_kind, from_id.as_str()), (to_kind, to_id.as_str())];
        locks.sort_unstable();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        for (kind, id) in locks {
            Self::lock_owner(&mut tx, kind, id)
                .await
                .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        }
        Self::lock_artifact(&mut tx, namespace, artifact_ref)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let retired: bool = sqlx::query_scalar(artifact_sql().retirements.select_is_retired.sql())
            .bind(to_kind)
            .bind(&to_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        if retired {
            return Err(ArtifactStoreFailure::DestinationOwnerRetired);
        }
        let source_exists: bool =
            sqlx::query_scalar(artifact_sql().owners.select_edge_exists.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .bind(from_kind)
                .bind(&from_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        if !source_exists {
            let destination_exists: bool =
                sqlx::query_scalar(artifact_sql().owners.select_edge_exists.sql())
                    .bind(namespace)
                    .bind(artifact_ref)
                    .bind(to_kind)
                    .bind(&to_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
            if destination_exists {
                tx.commit()
                    .await
                    .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
                return Ok(());
            }
            return Err(ArtifactStoreFailure::StagingEdgeMissing {
                artifact: format!("artifact `{artifact_ref}`"),
            });
        }
        sqlx::query(artifact_sql().owners.insert_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(to_kind)
            .bind(&to_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        sqlx::query(artifact_sql().owners.delete_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(from_kind)
            .bind(from_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))
    }

    async fn release_namespaced_owner(
        &self,
        namespace: &str,
        artifact_ref: &str,
        owner: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), ArtifactStoreFailure> {
        let (owner_kind, owner_id) = owner
            .storage_parts()
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        Self::lock_owner(&mut tx, owner_kind, &owner_id)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        Self::lock_artifact(&mut tx, namespace, artifact_ref)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        sqlx::query(artifact_sql().owners.delete_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(owner_kind)
            .bind(owner_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        sqlx::query(artifact_sql().owners_postgres.delete_unowned_artifact.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))
    }

    async fn retire_namespaced_owner(
        &self,
        namespace: &str,
        owner: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), ArtifactStoreFailure> {
        if !matches!(owner, lash_core_execution::ArtifactOwner::Execution(_)) {
            return Err(ArtifactStoreFailure::Backend(
                "only execution artifact owners can be retired".to_string(),
            ));
        }
        let (owner_kind, owner_id) = owner
            .storage_parts()
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        Self::lock_owner(&mut tx, owner_kind, &owner_id)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        sqlx::query(artifact_sql().retirements.insert_retirement.sql())
            .bind(owner_kind)
            .bind(&owner_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        let mut artifact_refs: Vec<String> =
            sqlx::query_scalar(artifact_sql().owners_postgres.select_owned_refs.sql())
                .bind(namespace)
                .bind(owner_kind)
                .bind(&owner_id)
                .fetch_all(&mut *tx)
                .await
                .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        artifact_refs.dedup();
        for artifact_ref in &artifact_refs {
            Self::lock_artifact(&mut tx, namespace, artifact_ref)
                .await
                .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        }
        sqlx::query(artifact_sql().owners.delete_owner_edges.sql())
            .bind(namespace)
            .bind(owner_kind)
            .bind(owner_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        sqlx::query(
            artifact_sql()
                .owners_postgres
                .delete_unowned_artifacts_for_owner
                .sql(),
        )
        .bind(namespace)
        .bind(&artifact_refs)
        .execute(&mut *tx)
        .await
        .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| ArtifactStoreFailure::Backend(error.to_string()))
    }
}

#[cfg(feature = "lashlang")]
#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for PostgresLashlangArtifactStore {
    fn pause_next_publication_for_testing(&self) -> Option<lashlang::ArtifactPublicationPause> {
        let pause = lashlang::ArtifactPublicationPause::default();
        *self
            .publication_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pause.clone());
        Some(pause)
    }

    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact.module_ref().as_str()) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let bytes = artifact
            .to_store_bytes()
            .map_err(lashlang::ArtifactStoreError::from)?;
        let publication_pause = self
            .publication_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pause) = publication_pause {
            pause.pause().await;
        }
        self.publish_namespaced_bytes(
            MODULE_ARTIFACT_NAMESPACE,
            artifact.module_ref().as_str(),
            &bytes,
            owner,
        )
        .await
        .map_err(ArtifactStoreFailure::into_artifact_store_error)
    }

    async fn retain_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.retain_namespaced_bytes(MODULE_ARTIFACT_NAMESPACE, module_ref.as_str(), owner)
            .await
            .map_err(ArtifactStoreFailure::into_artifact_store_error)
    }

    async fn transfer_module_artifact(
        &self,
        from: &lash_core_execution::ArtifactOwner,
        to: &lash_core_execution::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.transfer_namespaced_owner(MODULE_ARTIFACT_NAMESPACE, module_ref.as_str(), from, to)
            .await
            .map_err(ArtifactStoreFailure::into_artifact_store_error)
    }

    async fn release_module_artifact(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.release_namespaced_owner(MODULE_ARTIFACT_NAMESPACE, module_ref.as_str(), owner)
            .await
            .map_err(ArtifactStoreFailure::into_artifact_store_error)
    }

    async fn retire_module_artifact_owner(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.retire_namespaced_owner(MODULE_ARTIFACT_NAMESPACE, owner)
            .await
            .map_err(ArtifactStoreFailure::into_artifact_store_error)
    }

    async fn get_module_artifact(
        &self,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<Option<Arc<lashlang::ModuleArtifact>>, lashlang::ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref.as_str()) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let bytes = self
            .get_namespaced_bytes(MODULE_ARTIFACT_NAMESPACE, module_ref.as_str())
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        bytes
            .map(|bytes| {
                lashlang::ModuleArtifact::from_store_bytes(&bytes)
                    .map(Arc::new)
                    .map_err(lashlang::ArtifactStoreError::from)
            })
            .transpose()
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessExecutionEnvStore for PostgresLashlangArtifactStore {
    async fn publish_process_execution_env(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core_execution::PluginError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(lash_core_execution::PluginError::Invoke(
                "invalid process execution environment reference".into(),
            ));
        }
        if !env_ref.matches_store_bytes(bytes) {
            return Err(lash_core_execution::PluginError::Session(format!(
                "process execution environment bytes do not match `{env_ref}`"
            )));
        }
        self.publish_namespaced_bytes(PROCESS_ENV_NAMESPACE, env_ref.as_str(), bytes, owner)
            .await
            .map_err(ArtifactStoreFailure::into_plugin_error)
    }

    async fn transfer_process_execution_env(
        &self,
        from: &lash_core_execution::ArtifactOwner,
        to: &lash_core_execution::ArtifactOwner,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.transfer_namespaced_owner(PROCESS_ENV_NAMESPACE, env_ref.as_str(), from, to)
            .await
            .map_err(ArtifactStoreFailure::into_plugin_error)
    }

    async fn release_process_execution_env(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.release_namespaced_owner(PROCESS_ENV_NAMESPACE, env_ref.as_str(), owner)
            .await
            .map_err(ArtifactStoreFailure::into_plugin_error)
    }

    async fn retire_process_execution_env_owner(
        &self,
        owner: &lash_core_execution::ArtifactOwner,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.retire_namespaced_owner(PROCESS_ENV_NAMESPACE, owner)
            .await
            .map_err(ArtifactStoreFailure::into_plugin_error)
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core_execution::PluginError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(lash_core_execution::PluginError::Invoke(
                "invalid process execution environment reference".into(),
            ));
        }
        self.get_namespaced_bytes(PROCESS_ENV_NAMESPACE, env_ref.as_str())
            .await
            .map_err(|err| lash_core_execution::PluginError::Session(err.to_string()))
    }
}
