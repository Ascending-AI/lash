use lash_core::attachments::*;
use lash_core::store::SessionCatalogStore;
use lash_core::{AttachmentCreateMeta, AttachmentId, AttachmentRef, MediaType};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct InterleavedPages {
    backend: Arc<dyn AttachmentStore>,
    session: Arc<RuntimeAttachmentStore>,
    late: Vec<u8>,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl AttachmentStore for InterleavedPages {
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.backend.put(bytes, meta).await
    }
    async fn get(
        &self,
        id: &AttachmentId,
        max_bytes: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
        self.backend.get(id, max_bytes).await
    }
    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.backend.delete(id).await
    }
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.backend.head(id).await
    }
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        panic!("reclamation must never collect the whole blob namespace")
    }
    async fn list_page(
        &self,
        after: Option<&AttachmentId>,
        _: std::num::NonZeroU32,
    ) -> Result<AttachmentBlobPage, AttachmentStoreError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            // The preceding page's cursor object has been reclaimed. A new
            // upload intent lands after the sweep's root snapshot, before the
            // following page is read. The SQLite condemnation authority must
            // see that root, even though the initial snapshot cannot.
            assert!(self.backend.head(after.unwrap()).await?.is_none());
            self.session.put(self.late.clone(), meta()).await?;
        }
        self.backend
            .list_page(after, std::num::NonZeroU32::MIN)
            .await
    }
}
fn meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None)
}

#[tokio::test]
async fn paged_reclamation_keeps_rooted_blobs_across_delete_and_put_between_pages() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let factory = stores.session_store_factory();
    let session_id = lash_vocabulary::identity::SessionId::fixture("paged-gc");
    factory
        .admit_session(&lash_core::SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            owning_process_id: None,
            config: lash_core::PersistedSessionConfig::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
            head: lash_core::SessionCreationHead::Config,
            retention: lash_core::store::Retention::UntilGc,
        })
        .await
        .unwrap();
    let backend: Arc<dyn AttachmentStore> = stores.attachment_store();
    let session = Arc::new(RuntimeAttachmentStore::new(
        backend.clone(),
        Arc::new(PersistenceReferrersAdapter(factory.clone())),
        lash_vocabulary::identity::RuntimeOwner::Session(session_id),
        AttachmentPolicy::standard(),
    ));
    let mut contents = [b"first".to_vec(), b"second".to_vec(), b"third".to_vec()];
    contents.sort_by_key(|bytes| content_id(bytes));
    let orphan = backend.put(contents[0].clone(), meta()).await.unwrap();
    let rooted = session.put(contents[1].clone(), meta()).await.unwrap();
    let late = backend.put(contents[2].clone(), meta()).await.unwrap();
    let pages = InterleavedPages {
        backend: backend.clone(),
        session,
        late: contents[2].clone(),
        calls: AtomicUsize::new(0),
    };
    let report = reclaim_unreferenced_attachments(
        factory.as_ref(),
        &pages,
        AttachmentReclamationPolicy::new(0, EmptyRootSetPolicy::Refuse),
    )
    .await
    .unwrap();
    assert_eq!(report.scanned_blob_count, 3);
    assert_eq!(report.reclaimed_count, 1);
    assert!(report.failed_ids.is_empty());
    assert!(report.deleted_while_referenced.is_empty());
    assert!(backend.head(&orphan.id).await.unwrap().is_none());
    assert!(backend.head(&rooted.id).await.unwrap().is_some());
    assert!(backend.head(&late.id).await.unwrap().is_some());
    assert_eq!(pages.calls.load(Ordering::SeqCst), 4);
}
