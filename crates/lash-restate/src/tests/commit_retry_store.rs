//! A pass-through store over the shared in-memory recovery store that counts
//! drive seals, for the laws that probe a commit retry.

use super::*;

pub(super) struct CommitRetryStore {
    pub(super) inner: lash_core::store::SessionStore,
    pub(super) drive_seal_count: Arc<AtomicUsize>,
}

impl CommitRetryStore {
    pub(super) fn new(inner: lash_core::store::SessionStore) -> Self {
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
impl lash_core::store::RuntimePersistenceDecorator for CommitRetryStore {
    fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn load_session(
        &self,
    ) -> Result<Option<lash_core::store::PersistedSessionRead>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_session_head_meta(
        &self,
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
        self.inner
            .seal_drive_epoch(session_id, admission, observed_epoch, root_start)
            .await
    }
}
