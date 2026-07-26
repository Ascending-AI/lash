use crate::*;

/// Logical keyspaces multiplexed onto `lash_lashlang_artifacts`. Each namespace
/// owns its own half of the `(namespace, artifact_ref)` composite primary key,
/// so a key value that happens to collide across namespaces (a module ref that
/// equals a process-execution-env ref, say) resolves to independent rows instead
/// of silently clobbering one another under last-writer-wins.
pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "lashlang_module";
pub(crate) const RAW_ARTIFACT_NAMESPACE: &str = "lashlang_artifact";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";
pub(crate) const CURRENT_TRIGGER_MANIFEST_NAMESPACE: &str = "lashlang_trigger_manifest";

impl PostgresLashlangArtifactStore {
    async fn put_namespaced_bytes(
        &self,
        namespace: &str,
        artifact_ref: &str,
        bytes: &[u8],
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO lash_lashlang_artifacts (namespace, artifact_ref, artifact_bytes)
             VALUES ($1, $2, $3)
             ON CONFLICT (namespace, artifact_ref)
             DO UPDATE SET artifact_bytes = EXCLUDED.artifact_bytes",
        )
        .bind(namespace)
        .bind(artifact_ref)
        .bind(bytes)
        .execute(&self.pool)
        .await
        .map(|_| ())
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
}

#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for PostgresLashlangArtifactStore {
    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn put_module_artifact(
        &self,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        let bytes = artifact
            .to_store_bytes()
            .map_err(lashlang::ArtifactStoreError::from)?;
        self.put_namespaced_bytes(
            MODULE_ARTIFACT_NAMESPACE,
            artifact.module_ref.as_str(),
            &bytes,
        )
        .await
        .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))
    }

    async fn get_module_artifact(
        &self,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<Option<Arc<lashlang::ModuleArtifact>>, lashlang::ArtifactStoreError> {
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

    async fn replace_current_trigger_manifest(
        &self,
        owner_namespace: &str,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<lashlang::TriggerManifestReplacement, lashlang::ArtifactStoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "{CURRENT_TRIGGER_MANIFEST_NAMESPACE}:{owner_namespace}"
            ))
            .execute(&mut *tx)
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        let previous_bytes: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT artifact_bytes FROM lash_lashlang_artifacts
             WHERE namespace = $1 AND artifact_ref = $2",
        )
        .bind(CURRENT_TRIGGER_MANIFEST_NAMESPACE)
        .bind(owner_namespace)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        let previous = previous_bytes
            .map(|bytes| {
                serde_json::from_slice::<lashlang::CurrentTriggerKeyManifest>(&bytes)
                    .map_err(|err| lashlang::ArtifactStoreError::Decode(err.to_string()))
            })
            .transpose()?;
        let current = lashlang::CurrentTriggerKeyManifest {
            module_ref: artifact.module_ref.clone(),
            manifest: artifact.trigger_key_manifest.clone(),
        };
        let current_bytes = serde_json::to_vec(&current)
            .map_err(|err| lashlang::ArtifactStoreError::Encode(err.to_string()))?;
        sqlx::query(
            "INSERT INTO lash_lashlang_artifacts (namespace, artifact_ref, artifact_bytes)
             VALUES ($1, $2, $3)
             ON CONFLICT (namespace, artifact_ref)
             DO UPDATE SET artifact_bytes = EXCLUDED.artifact_bytes",
        )
        .bind(CURRENT_TRIGGER_MANIFEST_NAMESPACE)
        .bind(owner_namespace)
        .bind(current_bytes)
        .execute(&mut *tx)
        .await
        .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        tx.commit()
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        Ok(lashlang::TriggerManifestReplacement {
            previous_module_ref: previous.as_ref().map(|entry| entry.module_ref.clone()),
            current_module_ref: artifact.module_ref.clone(),
            diff: previous
                .map(|entry| entry.manifest.diff(&artifact.trigger_key_manifest))
                .unwrap_or_default(),
        })
    }

    async fn get_current_trigger_manifest(
        &self,
        owner_namespace: &str,
    ) -> Result<Option<lashlang::CurrentTriggerKeyManifest>, lashlang::ArtifactStoreError> {
        self.get_namespaced_bytes(CURRENT_TRIGGER_MANIFEST_NAMESPACE, owner_namespace)
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|err| lashlang::ArtifactStoreError::Decode(err.to_string()))
            })
            .transpose()
    }

    async fn put_artifact_bytes(
        &self,
        artifact_ref: &str,
        _descriptor: &str,
        bytes: &[u8],
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.put_namespaced_bytes(RAW_ARTIFACT_NAMESPACE, artifact_ref, bytes)
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))
    }

    async fn get_artifact_bytes(
        &self,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, lashlang::ArtifactStoreError> {
        self.get_namespaced_bytes(RAW_ARTIFACT_NAMESPACE, artifact_ref)
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for PostgresLashlangArtifactStore {
    fn durability_tier(&self) -> lash_core::DurabilityTier {
        lash_core::DurabilityTier::Durable
    }

    async fn put_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        self.put_namespaced_bytes(PROCESS_ENV_NAMESPACE, env_ref.as_str(), bytes)
            .await
            .map_err(|err| lash_core::PluginError::Session(err.to_string()))
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
        self.get_namespaced_bytes(PROCESS_ENV_NAMESPACE, env_ref.as_str())
            .await
            .map_err(|err| lash_core::PluginError::Session(err.to_string()))
    }
}
