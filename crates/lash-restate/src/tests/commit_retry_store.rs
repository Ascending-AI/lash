//! A pass-through store over the shared in-memory recovery store that counts
//! drive seals, for the laws that probe a commit retry.

use super::*;

pub(super) struct CommitRetryStore {
    pub(super) inner: Arc<dyn lash_core::RuntimeStore>,
    pub(super) drive_seal_count: Arc<AtomicUsize>,
}

impl CommitRetryStore {
    pub(super) fn new(inner: Arc<dyn lash_core::RuntimeStore>) -> Self {
        Self {
            inner,
            drive_seal_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

// Pass-through wrapper over the shared in-memory recovery store: it hides the
// persisted head from the retrying turn and counts drive seals; every other
// operation delegates to `inner`.
#[async_trait::async_trait]
impl lash_core::store::RuntimeStoreDecorator for CommitRetryStore {
    type Inner = dyn lash_core::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn load_session_window(
        &self,
        _session_id: &SessionId,
        _selector: lash_core::store::WindowSelector,
    ) -> Result<Option<lash_core::store::SessionWindowRead>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_session_head_meta(
        &self,
        _session_id: &SessionId,
    ) -> Result<Option<lash_core::store::SessionHeadMeta>, lash_core::StoreError> {
        Ok(None)
    }

    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &lash_core::store::AdmissionId,
        observed_epoch: u64,
        root_start: &lash_core::store::RootStartNonce,
    ) -> Result<lash_core::store::DriveEpochSeal, lash_core::StoreError> {
        self.drive_seal_count.fetch_add(1, Ordering::SeqCst);
        lash_core::store::DriveEpochStore::seal_drive_epoch(
            self.inner.as_ref(),
            session_id,
            admission,
            observed_epoch,
            root_start,
        )
        .await
    }
}
