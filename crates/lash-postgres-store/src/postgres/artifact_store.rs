use crate::*;

/// Logical keyspaces multiplexed onto `lash_lashlang_artifacts`. Each namespace
/// owns its own half of the `(namespace, artifact_ref)` composite primary key,
/// so a key value that happens to collide across namespaces (a module ref that
/// equals a process-execution-env ref, say) resolves to independent rows instead
/// of silently clobbering one another under last-writer-wins.
pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "lashlang_module";
#[cfg(feature = "lashlang")]
const RAW_ARTIFACT_NAMESPACE: &str = "lashlang_artifact";
const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";

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

#[cfg(feature = "lashlang")]
#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for PostgresLashlangArtifactStore {
    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn put_module_artifact(
        &self,
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

    async fn put_artifact_bytes(
        &self,
        artifact_ref: &str,
        _descriptor: &str,
        bytes: &[u8],
    ) -> Result<(), lashlang::ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact_ref) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid artifact namespace key".into(),
            ));
        }
        self.put_namespaced_bytes(RAW_ARTIFACT_NAMESPACE, artifact_ref, bytes)
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))
    }

    async fn get_artifact_bytes(
        &self,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, lashlang::ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact_ref) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid artifact namespace key".into(),
            ));
        }
        self.get_namespaced_bytes(RAW_ARTIFACT_NAMESPACE, artifact_ref)
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for PostgresLashlangArtifactStore {
    async fn put_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(lash_core::PluginError::Invoke(
                "invalid process execution environment reference".into(),
            ));
        }
        self.put_namespaced_bytes(PROCESS_ENV_NAMESPACE, env_ref.as_str(), bytes)
            .await
            .map_err(|err| lash_core::PluginError::Session(err.to_string()))
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
