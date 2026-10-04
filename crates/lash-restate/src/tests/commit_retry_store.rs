//! A pass-through store that counts shift seals for commit replay laws.

use super::*;

pub(super) struct CommitRetryStore {
    pub(super) inner: Arc<dyn lash_core::RuntimeStore>,
    pub(super) shift_seal_count: Arc<AtomicUsize>,
}

impl CommitRetryStore {
    pub(super) fn new(inner: Arc<dyn lash_core::RuntimeStore>) -> Self {
        Self {
            inner,
            shift_seal_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

// Retain the real admitted base while counting live shift seals. Every other
// operation delegates to `inner`.
#[async_trait::async_trait]
impl lash_core::store::RuntimeStoreDecorator for CommitRetryStore {
    type Inner = dyn lash_core::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn seal_shift_epoch(
        &self,
        session_id: &SessionId,
        admission: &lash_core::store::AdmissionId,
        observed_epoch: u64,
        run_start: &lash_core::store::RunStartNonce,
        hold: Option<&lash_core::store::RunHold>,
    ) -> Result<lash_core::store::ShiftEpochSeal, lash_core::StoreError> {
        self.shift_seal_count.fetch_add(1, Ordering::SeqCst);
        lash_core::store::ShiftEpochStore::seal_shift_epoch(
            self.inner.as_ref(),
            session_id,
            admission,
            observed_epoch,
            run_start,
            hold,
        )
        .await
    }
}
