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

/// Everything one host connects to PostgreSQL with: the storage, the live
/// replay store when the configuration asks for one, the configuration as
/// it took effect, and per-role pool metrics.
///
/// Build the durable backend from it with
/// [`DurableBackendBuilder::postgres`](crate::durable::DurableBackendBuilder::postgres),
/// which takes the durable settings from `effective_config.node` and from
/// nowhere else.
pub struct PostgresHost {
    /// The storage every store port of the database shares.
    pub storage: PostgresStorage,
    /// The live replay store, when `live_replay` is configured.
    pub live_replay: Option<Arc<PostgresLiveReplayStore>>,
    /// The validated configuration the storage runs under.
    pub effective_config: PostgresHostConfig,
    /// Each role pool's state.
    pub pool_metrics: PostgresPoolMetrics,
}

impl std::fmt::Debug for PostgresHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresHost")
            .field("catalog", &self.storage.catalog_id())
            .field("live_replay", &self.live_replay.is_some())
            .field("pool_metrics", &self.pool_metrics)
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
}

impl PostgresHost {
    /// Validate `config`, check its deployment budget against the server,
    /// open the storage's role pools and, when configured, the live replay
    /// store, all through `endpoints`.
    ///
    /// # Errors
    ///
    /// [`PostgresHostConnectError`] naming what refused.
    pub async fn connect(
        endpoints: &PostgresEndpoints,
        config: &PostgresHostConfig,
        observer: StoreObserver,
    ) -> Result<Self, PostgresHostConnectError> {
        let storage = PostgresStorage::connect(endpoints, config, observer).await?;
        let live_replay = match config.live_replay {
            Some(_) => Some(Arc::new(
                PostgresLiveReplayStore::connect(endpoints, config).await?,
            )),
            None => None,
        };
        Ok(Self {
            effective_config: storage.effective_config().clone(),
            pool_metrics: storage.pool_metrics(),
            storage,
            live_replay,
        })
    }
}
