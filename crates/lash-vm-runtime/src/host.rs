//! The parent's answers to a worker-hosted machine's host reads.

use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer, Timestamp};
use lash_vm_broker::{DurableSnapshotStore, ParentFault};
use lash_vm_client::RunHost;

use crate::ProjectionCatalog;

/// One run's host on the parent: the store's clock, the embedder's random
/// source, the backend's projection providers, and where prints go.
pub struct ParentHost {
    /// The run's store, whose clock is the run's clock.
    pub store: Arc<DurableSnapshotStore>,
    pub random: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub providers: ProjectionCatalog,
    /// Takes each printed value, in order.
    pub printed: Arc<dyn Fn(Datum) + Send + Sync>,
}

#[async_trait::async_trait]
impl RunHost for ParentHost {
    async fn clock(&self) -> Result<Timestamp, ParentFault> {
        let now = self
            .store
            .context()
            .durable_now()
            .await
            .map_err(|error| ParentFault(error.to_string()))?;
        Ok(Timestamp {
            nanoseconds: Integer::from(now.0.saturating_mul(1_000_000)),
        })
    }

    async fn random(&self) -> Result<u64, ParentFault> {
        Ok((self.random)())
    }

    async fn read(
        &self,
        handle: &Handle,
        request: &Datum,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        Ok(self.providers.answer(handle, request).await)
    }

    fn print(&self, value: Datum) -> Result<(), ParentFault> {
        (self.printed)(value);
        Ok(())
    }
}
