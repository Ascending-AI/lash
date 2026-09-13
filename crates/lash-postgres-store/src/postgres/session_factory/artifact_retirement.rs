use super::*;

impl PostgresSessionStoreFactory {
    pub(super) fn store_for(&self, session_id: SessionId) -> PostgresSessionStore {
        PostgresSessionStore {
            pool: self.pool.clone(),
            clock: Arc::clone(&self.clock),
            session_id,
            #[cfg(any(test, feature = "testing"))]
            lease_clock_for_testing: self.lease_clock_for_testing.clone(),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub(super) async fn resume_artifact_owner_retirements(
        &self,
    ) -> Result<(), lash_core::StoreError> {
        let effect_host = self
            .effect_host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let artifact_stores = self
            .artifact_stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let (Some(effect_host), Some((process_env_store, process_engines))) =
            (effect_host, artifact_stores)
        else {
            return Ok(());
        };
        let scopes = effect_host
            .pending_artifact_owner_retirements()
            .await
            .map_err(|error| lash_core::StoreError::Backend(error.to_string()))?;
        for scope in scopes {
            let owner = lash_core::ArtifactOwner::execution(scope.clone());
            process_env_store
                .retire_process_execution_env_owner(&owner)
                .await
                .map_err(|error| lash_core::StoreError::Backend(error.to_string()))?;
            process_engines
                .retire_artifact_owner(&owner)
                .await
                .map_err(|error| lash_core::StoreError::Backend(error.to_string()))?;
            effect_host
                .complete_artifact_owner_retirement(&scope)
                .await
                .map_err(|error| lash_core::StoreError::Backend(error.to_string()))?;
        }
        Ok(())
    }
}
