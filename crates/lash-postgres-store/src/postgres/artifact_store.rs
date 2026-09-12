use crate::*;

/// Logical keyspaces multiplexed onto `lash_lashlang_artifacts`.
pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "lashlang_module";
const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";

impl PostgresLashlangArtifactStore {
    async fn lock_owner(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        owner_kind: &str,
        owner_id: &str,
    ) -> Result<(), sqlx::Error> {
        let key = format!("lash-artifact-owner:{owner_kind}:{owner_id}");
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
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
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), String> {
        let (owner_kind, owner_id) = owner.storage_parts().map_err(|error| error.to_string())?;
        let mut tx = self.pool.begin().await.map_err(|error| error.to_string())?;
        Self::lock_owner(&mut tx, owner_kind, &owner_id)
            .await
            .map_err(|error| error.to_string())?;
        let retired: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM lash_artifact_owner_retirements
                 WHERE owner_kind = $1 AND owner_id = $2
             )",
        )
        .bind(owner_kind)
        .bind(&owner_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        if retired {
            return Err("artifact owner has been permanently retired".to_string());
        }
        sqlx::query(
            "INSERT INTO lash_lashlang_artifacts (namespace, artifact_ref, artifact_bytes)
             VALUES ($1, $2, $3)
             ON CONFLICT (namespace, artifact_ref)
             DO NOTHING",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .bind(bytes)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        let stored: Vec<u8> = sqlx::query_scalar(
            "SELECT artifact_bytes FROM lash_lashlang_artifacts
             WHERE namespace = $1 AND artifact_ref = $2",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        if stored != bytes {
            return Err(format!(
                "artifact `{artifact_ref}` in namespace `{namespace}` is immutable"
            ));
        }
        sqlx::query(
            "INSERT INTO lash_artifact_owners
             (namespace, artifact_ref, owner_kind, owner_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .bind(owner_kind)
        .bind(owner_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())
    }

    async fn get_namespaced_bytes(
        &self,
        namespace: &str,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT artifact_bytes FROM lash_lashlang_artifacts
             WHERE namespace = $1 AND artifact_ref = $2",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .fetch_optional(&self.pool)
        .await
    }

    async fn retain_namespaced_bytes(
        &self,
        namespace: &str,
        artifact_ref: &str,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), String> {
        let bytes = self
            .get_namespaced_bytes(namespace, artifact_ref)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing artifact `{artifact_ref}`"))?;
        self.publish_namespaced_bytes(namespace, artifact_ref, &bytes, owner)
            .await
    }

    async fn transfer_namespaced_owner(
        &self,
        namespace: &str,
        artifact_ref: &str,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
    ) -> Result<(), String> {
        let (from_kind, from_id) = from.storage_parts().map_err(|error| error.to_string())?;
        let (to_kind, to_id) = to.storage_parts().map_err(|error| error.to_string())?;
        let mut locks = [(from_kind, from_id.as_str()), (to_kind, to_id.as_str())];
        locks.sort_unstable();
        let mut tx = self.pool.begin().await.map_err(|error| error.to_string())?;
        for (kind, id) in locks {
            Self::lock_owner(&mut tx, kind, id)
                .await
                .map_err(|error| error.to_string())?;
        }
        let retired: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM lash_artifact_owner_retirements
                 WHERE owner_kind = $1 AND owner_id = $2
             )",
        )
        .bind(to_kind)
        .bind(&to_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        if retired {
            return Err("artifact destination owner has been permanently retired".to_string());
        }
        let source_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM lash_artifact_owners
                 WHERE namespace = $1 AND artifact_ref = $2
                   AND owner_kind = $3 AND owner_id = $4
             )",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .bind(from_kind)
        .bind(&from_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        if !source_exists {
            let destination_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                     SELECT 1 FROM lash_artifact_owners
                     WHERE namespace = $1 AND artifact_ref = $2
                       AND owner_kind = $3 AND owner_id = $4
                 )",
            )
            .bind(namespace)
            .bind(artifact_ref)
            .bind(to_kind)
            .bind(&to_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
            if destination_exists {
                tx.commit().await.map_err(|error| error.to_string())?;
                return Ok(());
            }
            return Err(format!(
                "artifact `{artifact_ref}` is not retained by the staging owner"
            ));
        }
        sqlx::query(
            "INSERT INTO lash_artifact_owners
             (namespace, artifact_ref, owner_kind, owner_id)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .bind(to_kind)
        .bind(&to_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        sqlx::query(
            "DELETE FROM lash_artifact_owners
             WHERE namespace = $1 AND artifact_ref = $2
               AND owner_kind = $3 AND owner_id = $4",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .bind(from_kind)
        .bind(from_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())
    }

    async fn release_namespaced_owner(
        &self,
        namespace: &str,
        artifact_ref: &str,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), String> {
        let (owner_kind, owner_id) = owner.storage_parts().map_err(|error| error.to_string())?;
        let mut tx = self.pool.begin().await.map_err(|error| error.to_string())?;
        Self::lock_owner(&mut tx, owner_kind, &owner_id)
            .await
            .map_err(|error| error.to_string())?;
        sqlx::query(
            "DELETE FROM lash_artifact_owners
             WHERE namespace = $1 AND artifact_ref = $2
               AND owner_kind = $3 AND owner_id = $4",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .bind(owner_kind)
        .bind(owner_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        sqlx::query(
            "DELETE FROM lash_lashlang_artifacts AS artifact
             WHERE artifact.namespace = $1 AND artifact.artifact_ref = $2
               AND NOT EXISTS (
                   SELECT 1 FROM lash_artifact_owners AS owner
                   WHERE owner.namespace = artifact.namespace
                     AND owner.artifact_ref = artifact.artifact_ref
               )",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())
    }

    async fn retire_namespaced_owner(
        &self,
        namespace: &str,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), String> {
        if !matches!(owner, lash_core::ArtifactOwner::Execution(_)) {
            return Err("only execution artifact owners can be retired".to_string());
        }
        let (owner_kind, owner_id) = owner.storage_parts().map_err(|error| error.to_string())?;
        let mut tx = self.pool.begin().await.map_err(|error| error.to_string())?;
        Self::lock_owner(&mut tx, owner_kind, &owner_id)
            .await
            .map_err(|error| error.to_string())?;
        sqlx::query(
            "INSERT INTO lash_artifact_owner_retirements (owner_kind, owner_id)
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(owner_kind)
        .bind(&owner_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        sqlx::query(
            "DELETE FROM lash_artifact_owners
             WHERE namespace = $1 AND owner_kind = $2 AND owner_id = $3",
        )
        .bind(namespace)
        .bind(owner_kind)
        .bind(owner_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        sqlx::query(
            "DELETE FROM lash_lashlang_artifacts AS artifact
             WHERE artifact.namespace = $1
               AND NOT EXISTS (
                   SELECT 1 FROM lash_artifact_owners AS owner
                   WHERE owner.namespace = artifact.namespace
                     AND owner.artifact_ref = artifact.artifact_ref
               )",
        )
        .bind(namespace)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())
    }
}

#[cfg(feature = "lashlang")]
#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for PostgresLashlangArtifactStore {
    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact.module_ref.as_str()) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let bytes = artifact
            .to_store_bytes()
            .map_err(lashlang::ArtifactStoreError::from)?;
        self.publish_namespaced_bytes(
            MODULE_ARTIFACT_NAMESPACE,
            artifact.module_ref.as_str(),
            &bytes,
            owner,
        )
        .await
        .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))
    }

    async fn retain_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.retain_namespaced_bytes(MODULE_ARTIFACT_NAMESPACE, module_ref.as_str(), owner)
            .await
            .map_err(lashlang::ArtifactStoreError::Backend)
    }

    async fn transfer_module_artifact(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.transfer_namespaced_owner(MODULE_ARTIFACT_NAMESPACE, module_ref.as_str(), from, to)
            .await
            .map_err(lashlang::ArtifactStoreError::Backend)
    }

    async fn release_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.release_namespaced_owner(MODULE_ARTIFACT_NAMESPACE, module_ref.as_str(), owner)
            .await
            .map_err(lashlang::ArtifactStoreError::Backend)
    }

    async fn retire_module_artifact_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.retire_namespaced_owner(MODULE_ARTIFACT_NAMESPACE, owner)
            .await
            .map_err(lashlang::ArtifactStoreError::Backend)
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
impl lash_core::ProcessExecutionEnvStore for PostgresLashlangArtifactStore {
    async fn publish_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(lash_core::PluginError::Invoke(
                "invalid process execution environment reference".into(),
            ));
        }
        if !env_ref.matches_store_bytes(bytes) {
            return Err(lash_core::PluginError::Session(format!(
                "process execution environment bytes do not match `{env_ref}`"
            )));
        }
        self.publish_namespaced_bytes(PROCESS_ENV_NAMESPACE, env_ref.as_str(), bytes, owner)
            .await
            .map_err(|err| lash_core::PluginError::Session(err.to_string()))
    }

    async fn transfer_process_execution_env(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core::PluginError> {
        self.transfer_namespaced_owner(PROCESS_ENV_NAMESPACE, env_ref.as_str(), from, to)
            .await
            .map_err(lash_core::PluginError::Session)
    }

    async fn release_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core::PluginError> {
        self.release_namespaced_owner(PROCESS_ENV_NAMESPACE, env_ref.as_str(), owner)
            .await
            .map_err(lash_core::PluginError::Session)
    }

    async fn retire_process_execution_env_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lash_core::PluginError> {
        self.retire_namespaced_owner(PROCESS_ENV_NAMESPACE, owner)
            .await
            .map_err(lash_core::PluginError::Session)
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(lash_core::PluginError::Invoke(
                "invalid process execution environment reference".into(),
            ));
        }
        self.get_namespaced_bytes(PROCESS_ENV_NAMESPACE, env_ref.as_str())
            .await
            .map_err(|err| lash_core::PluginError::Session(err.to_string()))
    }
}
