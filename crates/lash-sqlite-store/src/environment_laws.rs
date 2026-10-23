//! Environment owner laws on this backend's graph retirement and real cleanup relay.
use super::*;
use lash_durable::laws::environment::{self as env_laws, EnvironmentTier};
use lash_execution_contracts::backend::StoreSet;
#[async_trait::async_trait]
impl EnvironmentTier for Tier {
    fn packages(&self) -> Arc<dyn lash_durable::packages::PackageStore> {
        let stores: Arc<dyn StoreSet> = Arc::new(self._set.clone());
        stores.package_store()
    }
    async fn relay(&self) -> Result<(), String> {
        let stores: Arc<dyn StoreSet> = Arc::new(self._set.clone());
        let backend = lash_conformance::backend_over(stores);
        let relay = lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
            &backend,
            lash_core::ProcessEngineRegistry::default(),
        );
        let result = lash_core::runtime::obligations::relay::relay_due(
            &relay,
            backend.clock().as_ref(),
            std::num::NonZeroUsize::new(256).unwrap(),
        )
        .await
        .map_err(|e| e.to_string())?;
        if result.stalled != 0 {
            return Err(format!("cleanup stalled: {result:?}"));
        }
        Ok(())
    }
}
macro_rules! law {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let tier = super::tier(false).await;
            env_laws::$name(&tier).await.unwrap();
        }
    };
}
law!(env_lifetime);
law!(env_prepare);
law!(env_reincarnate);
law!(env_acquire_retire);
law!(env_crash_cuts_and_relay);
