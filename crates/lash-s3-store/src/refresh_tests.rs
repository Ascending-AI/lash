//! ATTACHMENT-REFRESH-WITHOUT-BODY-REWRITE (FIG-6602): a repeated put of
//! content already held refreshes its freshness without uploading the
//! immutable body again, and the fenced delete-time HEAD observes the refresh
//! even when listing is stale.

use super::*;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use lash_core::facade_support::reclaim_unreferenced_attachments;
use lash_core::{AttachmentReclamationPolicy, EmptyRootSetPolicy};
use object_store::list::PaginatedListResult;
use object_store::memory::InMemory;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    PutMultipartOptions, PutOptions, PutResult,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const HOUR_MS: u64 = 60 * 60 * 1000;
const GRACE_MS: u64 = 10 * 60 * 1000;

/// An object store that counts uploaded bytes, dates each put in a
/// controllable past, and can serve a listing captured earlier.
#[derive(Debug)]
struct InstrumentedObjects {
    inner: InMemory,
    uploaded_bytes: AtomicU64,
    /// How far in the past the next put lands.
    backdate_ms: AtomicU64,
    /// Each object's backdate, fixed when it was put.
    ages: Mutex<HashMap<Path, u64>>,
    /// Time passed since every put, added to every reported age.
    elapsed_ms: AtomicU64,
    /// A sweep's stale snapshot: listings answer from it while set.
    frozen_listing: Mutex<Option<Vec<ObjectMeta>>>,
    /// A delete that lands just before the next freshness marker does.
    delete_before_refresh: Mutex<Option<Path>>,
}

impl std::fmt::Display for InstrumentedObjects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstrumentedObjects")
    }
}

impl InstrumentedObjects {
    fn new() -> Self {
        Self {
            inner: InMemory::new(),
            uploaded_bytes: AtomicU64::new(0),
            backdate_ms: AtomicU64::new(0),
            ages: Mutex::new(HashMap::new()),
            elapsed_ms: AtomicU64::new(0),
            frozen_listing: Mutex::new(None),
            delete_before_refresh: Mutex::new(None),
        }
    }

    fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes.load(Ordering::SeqCst)
    }

    fn ages(&self) -> (HashMap<Path, u64>, u64) {
        (
            self.ages.lock().unwrap().clone(),
            self.elapsed_ms.load(Ordering::SeqCst),
        )
    }

    async fn freeze_listing(&self) {
        let listing: Vec<_> = self.list(None).try_collect().await.unwrap();
        *self.frozen_listing.lock().unwrap() = Some(listing);
    }

    fn thaw_listing(&self) {
        *self.frozen_listing.lock().unwrap() = None;
    }

    async fn raw_keys(&self) -> Vec<Path> {
        self.inner
            .list(None)
            .map_ok(|meta| meta.location)
            .try_collect()
            .await
            .unwrap()
    }
}

#[async_trait::async_trait]
impl ObjectStore for InstrumentedObjects {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let racing_delete = location
            .as_ref()
            .ends_with(FRESHNESS_SUFFIX)
            .then(|| self.delete_before_refresh.lock().unwrap().take())
            .flatten();
        if let Some(body) = racing_delete {
            self.inner.delete(&body).await?;
        }
        self.uploaded_bytes
            .fetch_add(payload.content_length() as u64, Ordering::SeqCst);
        self.ages
            .lock()
            .unwrap()
            .insert(location.clone(), self.backdate_ms.load(Ordering::SeqCst));
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        _location: &Path,
        _opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        unimplemented!("attachment puts are single requests")
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let mut result = self.inner.get_opts(location, options).await?;
        let (ages, elapsed) = self.ages();
        result.meta = aged(result.meta, &ages, elapsed);
        Ok(result)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        if let Some(frozen) = self.frozen_listing.lock().unwrap().clone() {
            let prefix = prefix.cloned();
            return futures_util::stream::iter(
                frozen
                    .into_iter()
                    .filter(move |meta| {
                        prefix
                            .as_ref()
                            .is_none_or(|prefix| meta.location.prefix_matches(prefix))
                    })
                    .map(Ok),
            )
            .boxed();
        }
        let (ages, elapsed) = self.ages();
        self.inner
            .list(prefix)
            .map_ok(move |meta| aged(meta, &ages, elapsed))
            .boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

fn aged(mut meta: ObjectMeta, ages: &HashMap<Path, u64>, elapsed_ms: u64) -> ObjectMeta {
    let age = ages.get(&meta.location).copied().unwrap_or(0) + elapsed_ms;
    meta.last_modified -= std::time::Duration::from_millis(age);
    meta
}

/// Key-ordered pages with a continuation token, as general-purpose S3 lists.
#[async_trait::async_trait]
impl PaginatedListStore for InstrumentedObjects {
    async fn list_paginated(
        &self,
        prefix: Option<&str>,
        opts: PaginatedListOptions,
    ) -> object_store::Result<PaginatedListResult> {
        let limit = opts.max_keys.unwrap();
        let mut objects: Vec<_> = self.list(None).try_collect().await?;
        objects.retain(|meta| {
            prefix.is_none_or(|prefix| meta.location.as_ref().starts_with(prefix))
                && opts
                    .offset
                    .as_deref()
                    .is_none_or(|offset| meta.location.as_ref() > offset)
        });
        objects.sort_by(|a, b| a.location.cmp(&b.location));
        let page_token = (objects.len() > limit).then(|| "more".to_owned());
        objects.truncate(limit);
        Ok(PaginatedListResult {
            result: ListResult {
                objects,
                common_prefixes: vec![],
                extensions: Default::default(),
            },
            page_token,
        })
    }
}

fn meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(
        lash_core::MediaType::parse("image/png").unwrap(),
        None,
        None,
    )
}

fn epoch_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn store_over(objects: &Arc<InstrumentedObjects>, paginated: bool) -> S3AttachmentStore {
    let mut store = S3AttachmentStore::from_object_store(objects.clone(), Some("refresh".into()));
    if paginated {
        store.pages = Some(objects.clone());
    }
    store
}

async fn refresh_survives_a_stale_listing_and_later_reclaims(paginated: bool) {
    let objects = Arc::new(InstrumentedObjects::new());
    let store = store_over(&objects, paginated);
    let bytes = vec![7u8; 256 * 1024];
    let policy = AttachmentReclamationPolicy::new(GRACE_MS, EmptyRootSetPolicy::AuthorizeDeleteAll);

    // The first put landed an hour ago; a sweep then snapshots the old body.
    objects.backdate_ms.store(HOUR_MS, Ordering::SeqCst);
    let first = store.put(bytes.clone(), meta()).await.unwrap();
    objects.freeze_listing().await;
    objects.backdate_ms.store(0, Ordering::SeqCst);
    let second = store.put(bytes.clone(), meta()).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(
        objects.uploaded_bytes(),
        bytes.len() as u64,
        "a repeated put of held content transfers its body once"
    );
    let fresh = store.head(&first.id).await.unwrap().unwrap();
    assert!(
        epoch_ms() - fresh.last_modified_epoch_ms.unwrap() < GRACE_MS,
        "head observes the refresh"
    );

    // The stale snapshot names old bytes; fenced HEAD observes the refresh.
    let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let fenced = lash_execution_contracts::backend::StoreSet::session_store_factory(&stores);
    let report = reclaim_unreferenced_attachments(fenced.as_ref(), &store, policy)
        .await
        .unwrap();
    assert_eq!((report.scanned_blob_count, report.reclaimed_count), (1, 0));
    assert_eq!(
        store.get(&first.id, u64::MAX).await.unwrap().bytes,
        bytes,
        "refreshed content survives"
    );

    // A live listing reports the refreshed freshness once per content id,
    // even when a one-object page splits the body from its marker.
    objects.thaw_listing();
    let page = store
        .list_page(None, std::num::NonZeroU32::MIN)
        .await
        .unwrap();
    assert_eq!(page.blobs, vec![fresh.clone()]);
    assert_eq!(page.next, Some(first.id.clone()));
    let rest = store
        .list_page(Some(&first.id), std::num::NonZeroU32::MIN)
        .await
        .unwrap();
    assert!(rest.blobs.is_empty() && rest.next.is_none());
    assert_eq!(store.list().await.unwrap(), vec![fresh]);

    // Once the refresh ages out, the content is reclaimable and the sweep
    // leaves neither the body nor its marker behind.
    objects.elapsed_ms.store(HOUR_MS, Ordering::SeqCst);
    let report = reclaim_unreferenced_attachments(fenced.as_ref(), &store, policy)
        .await
        .unwrap();
    assert_eq!(report.reclaimed_count, 1);
    assert!(objects.raw_keys().await.is_empty());
    assert_eq!(store.head(&first.id).await.unwrap(), None);
}

#[tokio::test]
async fn refresh_without_body_rewrite_over_ordered_pages() {
    refresh_survives_a_stale_listing_and_later_reclaims(true).await;
}

#[tokio::test]
async fn refresh_without_body_rewrite_over_unordered_listing() {
    refresh_survives_a_stale_listing_and_later_reclaims(false).await;
}

/// A delete that lands between a put's existence check and its refresh
/// cannot leave the put answering for bytes that are gone: the put re-checks
/// the body after refreshing and uploads it again.
#[tokio::test]
async fn a_refresh_that_loses_its_body_to_a_delete_uploads_it_again() {
    let objects = Arc::new(InstrumentedObjects::new());
    let store = store_over(&objects, false);
    let bytes = vec![3u8; 4096];
    let reference = store.put(bytes.clone(), meta()).await.unwrap();
    *objects.delete_before_refresh.lock().unwrap() =
        Some(store.content_path(&reference.id).unwrap());
    store.put(bytes.clone(), meta()).await.unwrap();
    assert_eq!(objects.uploaded_bytes(), 2 * bytes.len() as u64);
    assert_eq!(
        store.get(&reference.id, u64::MAX).await.unwrap().bytes,
        bytes
    );
}
