//! A pass-through store that counts atomic admissions for commit replay laws.

use super::*;

pub(super) struct CommitRetryStore {
    pub(super) inner: Arc<dyn lash_core::RuntimeStore>,
    pub(super) admission_count: Arc<AtomicUsize>,
}

impl CommitRetryStore {
    pub(super) fn new(inner: Arc<dyn lash_core::RuntimeStore>) -> Self {
        Self {
            inner,
            admission_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

// Retain the real admitted base while counting live atomic admissions. Every other
// operation delegates to `inner`.
#[async_trait::async_trait]
impl lash_core::store::RuntimeStoreDecorator for CommitRetryStore {
    type Inner = dyn lash_core::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn commit_shift_admission(
        &self,
        request: &lash_core::store::ShiftAdmissionWrite,
        anchor: &lash_trace::TraceAnchor,
    ) -> Result<lash_core::store::ShiftAdmissionReceipt, lash_core::StoreError> {
        self.admission_count.fetch_add(1, Ordering::SeqCst);
        self.inner.commit_shift_admission(request, anchor).await
    }
}
