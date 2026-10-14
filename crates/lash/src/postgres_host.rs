//! [`PostgresHost`]: a host's whole PostgreSQL wiring from one validated
//! [`PostgresHostConfig`] (FIG-5240).
//!
//! `docs/operations/postgres.md` is the guide: the configuration reference,
//! the sizing formula, the pooler rules and the migration from hand-built
//! pools.

use std::sync::Arc;

use lash_core::facade_support::StoreObserver;
use lash_postgres_store::{
    PostgresEndpoints, PostgresHostConfig, PostgresHostError, PostgresPoolMetrics, PostgresStorage,
};

use crate::postgres_live_replay::{PostgresLiveReplayError, PostgresLiveReplayStore};
use crate::postgres_process_replay::{PostgresProcessReplayError, PostgresProcessReplayStore};

/// Everything one host connects to PostgreSQL with: the storage, the live
/// replay and process replay stores when the configuration asks for them,
/// the configuration as it took effect, and per-role pool metrics.
///
/// Build the durable backend from it with
/// [`DurableBackendBuilder::postgres`](crate::durable::DurableBackendBuilder::postgres),
/// which takes the durable settings from [`PostgresHost::effective_config`] and from
/// nowhere else.
pub struct PostgresHost {
    /// The storage every store port of the database shares.
    storage: PostgresStorage,
    /// The live replay store, when `live_replay` is configured.
    live_replay: Option<Arc<PostgresLiveReplayStore>>,
    /// The process replay store, when `process_replay` is configured.
    process_replay: Option<Arc<PostgresProcessReplayStore>>,
}

impl std::fmt::Debug for PostgresHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresHost")
            .field("catalog", &self.storage.catalog_id())
            .field("live_replay", &self.live_replay.is_some())
            .field("process_replay", &self.process_replay.is_some())
            .field("pool_metrics", &self.storage.pool_metrics())
            .finish_non_exhaustive()
    }
}

/// Why a [`PostgresHost`] did not connect.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PostgresHostConnectError {
    /// The storage refused or failed: configuration, budget, endpoints or
    /// schema.
    #[error(transparent)]
    Storage(#[from] PostgresHostError),
    /// The live replay store refused or failed.
    #[error(transparent)]
    LiveReplay(#[from] PostgresLiveReplayError),
    /// The process replay store refused or failed.
    #[error(transparent)]
    ProcessReplay(#[from] PostgresProcessReplayError),
}

impl PostgresHost {
    /// The connected storage shared by every database store port.
    pub fn storage(&self) -> &PostgresStorage {
        &self.storage
    }

    /// Live replay over this host's connection policy, when configured.
    pub fn live_replay(&self) -> Option<&Arc<PostgresLiveReplayStore>> {
        self.live_replay.as_ref()
    }

    /// Process replay over this host's connection policy, when configured.
    pub fn process_replay(&self) -> Option<&Arc<PostgresProcessReplayStore>> {
        self.process_replay.as_ref()
    }

    /// The validated policy in effect on the connected storage.
    pub fn effective_config(&self) -> &PostgresHostConfig {
        self.storage.effective_config()
    }

    /// Each connected role pool's state.
    pub fn pool_metrics(&self) -> PostgresPoolMetrics {
        self.storage.pool_metrics()
    }

    /// Build the host over `storage` a caller already connected — through
    /// [`PostgresStorage::connect`] or [`PostgresStorage::from_pool_set`]
    /// over pools the caller built itself — opening the live replay and
    /// process replay stores the storage's effective configuration selects,
    /// through `endpoints`.
    ///
    /// # Errors
    ///
    /// [`PostgresHostConnectError`] naming what refused.
    pub async fn from_storage(
        storage: PostgresStorage,
        endpoints: &PostgresEndpoints,
    ) -> Result<Self, PostgresHostConnectError> {
        let config = storage.effective_config();
        let live_replay = match config.live_replay {
            Some(_) => Some(Arc::new(
                PostgresLiveReplayStore::connect(endpoints, config).await?,
            )),
            None => None,
        };
        let process_replay = match config.process_replay {
            Some(_) => Some(Arc::new(
                PostgresProcessReplayStore::connect(endpoints, config).await?,
            )),
            None => None,
        };
        Ok(Self {
            storage,
            live_replay,
            process_replay,
        })
    }

    /// Validate `config`, check its deployment budget against the server,
    /// open the storage's role pools and, when configured, the live replay
    /// and process replay stores, all through `endpoints`.
    ///
    /// # Errors
    ///
    /// [`PostgresHostConnectError`] naming what refused.
    pub async fn connect(
        endpoints: &PostgresEndpoints,
        config: &PostgresHostConfig,
        observer: StoreObserver,
    ) -> Result<Self, PostgresHostConnectError> {
        Self::from_storage(
            PostgresStorage::connect(endpoints, config, observer).await?,
            endpoints,
        )
        .await
    }
}
