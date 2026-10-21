//! The parent's answers to a worker-hosted machine's host reads.

use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorDatum, Handle};
use lash_vm_broker::{DurableSnapshotStore, ParentFault};
use lash_vm_client::RunHost;

use crate::ProjectionCatalog;

/// Who answers a run's read-bound `perform`s (`K-EFF-012`): a cell's
/// planned reads, through the providers its plan retained.
#[async_trait::async_trait]
pub trait EffectReads: Send + Sync {
    /// One read of `effect` with `args`, performed at `site` under the
    /// authored statement `origin`: its result, or the error raised at the
    /// `perform`. `Err` is the parent's own failure, and stops the run.
    async fn perform(
        &self,
        use_: &lash_vocabulary::OperationUse,
        args: &[Datum],
        site: &lash_kernel_doc::PositionLabel,
        origin: &lash_kernel_doc::Site,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault>;
}

/// One run's host on the parent: the store's clock, the embedder's random
/// source, the backend's projection providers, the run's reads, and where
/// prints go.
pub struct ParentHost {
    pub observation: lash_kernel_doc::RunObservation,
    /// Takes exact deltas from worker frames, outside checkpoint state.
    pub ran: Arc<dyn Fn(&lash_kernel_vm::SiteCounts) + Send + Sync>,
    /// The run's store, whose clock is the run's clock.
    pub store: Arc<DurableSnapshotStore>,
    pub providers: ProjectionCatalog,
    /// Answers the run's read-bound `perform`s.
    pub reads: Arc<dyn EffectReads>,
    /// Takes each printed value, in order.
    pub printed: Arc<dyn Fn(Datum) + Send + Sync>,
}

#[async_trait::async_trait]
impl RunHost for ParentHost {
    fn observe(&self) -> lash_kernel_vm::ObservationMode {
        self.observation
    }

    fn ran(&self, delta: &lash_kernel_vm::SiteCounts) {
        (self.ran)(delta);
    }

    async fn read(
        &self,
        handle: &Handle,
        request: &Datum,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        Ok(self.providers.answer(handle, request).await)
    }

    async fn perform(
        &self,
        use_: &lash_vocabulary::OperationUse,
        args: &[Datum],
        site: &lash_kernel_doc::PositionLabel,
        origin: &lash_kernel_doc::Site,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        self.reads.perform(use_, args, site, origin).await
    }

    fn print(&self, value: Datum) -> Result<(), ParentFault> {
        (self.printed)(value);
        Ok(())
    }
}
