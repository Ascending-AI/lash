use super::*;
use crate::SessionStoreFactory as _;
use pretty_assertions::assert_eq;
use std::sync::atomic::{AtomicBool, Ordering};

/// Which queue listing a [`DroppingQueueListing`] drops a batch from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listing {
    Total,
    Pending,
}

/// A store whose next listing of one kind omits its first batch: the seam
/// fault the model-agreement oracle must attribute.
struct DroppingQueueListing {
    inner: Arc<dyn RuntimePersistence>,
    listing: Listing,
    armed: AtomicBool,
}

impl DroppingQueueListing {
    fn drop_next(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn dropped(
        &self,
        listing: Listing,
        mut batches: Vec<crate::QueuedWorkBatch>,
    ) -> Vec<crate::QueuedWorkBatch> {
        if listing == self.listing
            && self.armed.swap(false, Ordering::SeqCst)
            && !batches.is_empty()
        {
            batches.remove(0);
        }
        batches
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for DroppingQueueListing {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::StoreError> {
        let batches = self.inner.list_queued_work(session_id).await?;
        Ok(self.dropped(Listing::Total, batches))
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::StoreError> {
        let batches = self.inner.list_pending_queued_work(session_id).await?;
        Ok(self.dropped(Listing::Pending, batches))
    }
}

/// A SQLite memory backend's store for the law's session, with one modeled
/// pending batch, behind a listing that drops a batch when armed.
async fn one_pending_batch_world(
    listing: Listing,
) -> (
    lash_sqlite_store::SqliteBackend,
    Arc<DroppingQueueListing>,
    ReferenceModel,
) {
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("memory backend");
    let inner = backend
        .session_store_factory()
        .create_store(&super::session_store_request(
            &SessionId::from(SESSION_ID),
            "runtime-persistence-model",
            crate::SessionRelation::Root,
        ))
        .await
        .expect("create the modeled session store");
    let store = Arc::new(DroppingQueueListing {
        inner,
        listing,
        armed: AtomicBool::new(false),
    });
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    apply_operation(
        store.as_ref(),
        None,
        &mut model,
        &mut shape,
        0,
        &RuntimePersistenceOp::EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
    )
    .await
    .expect("enqueue modeled batch");
    (backend, store, model)
}

#[tokio::test]
async fn assert_model_agreement_attributes_total_queue_store_seam_drop() {
    let (_backend, store, model) = one_pending_batch_world(Listing::Total).await;
    store.drop_next();

    assert_eq!(
        assert_model_agreement(store.as_ref(), &model).await,
        Err("queued-work state differs from the reference model".to_string())
    );
}

#[tokio::test]
async fn assert_model_agreement_attributes_pending_queue_store_seam_drop() {
    let (_backend, store, model) = one_pending_batch_world(Listing::Pending).await;
    store.drop_next();

    assert_eq!(
        assert_model_agreement(store.as_ref(), &model).await,
        Err("pending queued-work projection differs from live-claim model".to_string())
    );
}
