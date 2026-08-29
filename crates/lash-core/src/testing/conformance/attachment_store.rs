//! [`AttachmentStore`] conformance: content addressing, freshness, and round trips.

use super::*;

/// Run the full [`AttachmentStore`] conformance suite against the backend
/// produced by `make`. `make` must return a fresh, empty store on each call.
/// `expected_persistence` is the tier this backend declares (`Ephemeral` for
/// in-memory, `Durable` for persistent backends). The freshness law waits on the
/// real clock so it also works for backends whose timestamps have whole-second
/// resolution; do not run this suite with a frozen system clock.
pub async fn attachment_store<F>(make: F, expected_persistence: AttachmentStorePersistence)
where
    F: Fn() -> Arc<dyn AttachmentStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "attachment_store");
    drop((first, second));
    attachment_put_get_round_trips_bytes_and_meta(make()).await;
    attachment_is_content_addressed(make()).await;
    attachment_head_reflects_put_and_refreshes_timestamp(make()).await;
    attachment_get_unknown_is_not_found(make()).await;
    attachment_delete_removes_content_and_is_idempotent(make()).await;
    attachment_head_reports_absence(make()).await;
    attachment_list_enumerates_stored_blobs(make()).await;
    attachment_head_agrees_with_list(make()).await;
    attachment_reports_declared_persistence(make(), expected_persistence);
    session_attachment_store_enforces_host_size_limit(make).await;
}

async fn session_attachment_store_enforces_host_size_limit<F>(make: F)
where
    F: Fn() -> Arc<dyn AttachmentStore>,
{
    const MAX_ATTACHMENT_BYTES: u64 = 4;

    let bounded_backend = make();
    let bounded = crate::SessionAttachmentStore::ephemeral(Arc::clone(&bounded_backend))
        .with_max_attachment_bytes(Some(MAX_ATTACHMENT_BYTES));
    let error = bounded
        .put(vec![1; 5], attachment_meta())
        .await
        .expect_err("an attachment over the host limit must be rejected");
    assert!(
        matches!(
            error,
            AttachmentStoreError::SizeLimitExceeded {
                byte_len: 5,
                max_bytes: MAX_ATTACHMENT_BYTES,
            }
        ),
        "oversize rejection must retain the measured and configured byte counts, got {error:?}"
    );
    assert!(
        bounded_backend
            .list()
            .await
            .expect("list backend after rejected put")
            .is_empty(),
        "an oversize rejection must not write a backend blob"
    );

    let at_limit = bounded
        .put(vec![2; MAX_ATTACHMENT_BYTES as usize], attachment_meta())
        .await
        .expect("an attachment exactly at the host limit must succeed");
    assert_eq!(at_limit.byte_len, MAX_ATTACHMENT_BYTES);

    let unbounded = crate::SessionAttachmentStore::ephemeral(make());
    let default_put = unbounded
        .put(vec![3; 5], attachment_meta())
        .await
        .expect("the default attachment policy must remain unbounded");
    assert_eq!(default_put.byte_len, 5);
}

/// Run the full [`AttachmentStore`] suite plus durable reopen checks.
pub async fn attachment_store_reopenable<F>(
    make: F,
    expected_persistence: AttachmentStorePersistence,
) where
    F: Fn() -> ReopenableAttachmentStore,
{
    let probe = make();
    assert_fresh_instances(&probe.open, &probe.reopen, "attachment_store_reopenable");
    attachment_store(|| make().open, expected_persistence).await;
    attachment_store_survives_reopen(make()).await;
}

fn attachment_meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(
        MediaType::parse("image/png").unwrap(),
        Some(AttachmentTypeMetadata::image(Some(7), Some(11))),
        Some("pixel".to_string()),
    )
}

async fn attachment_put_get_round_trips_bytes_and_meta(store: Arc<dyn AttachmentStore>) {
    let bytes = vec![1u8, 2, 3, 4, 5];
    let reference = store
        .put(bytes.clone(), attachment_meta())
        .await
        .expect("put attachment");
    let stored = store.get(&reference.id).await.expect("get attachment");

    assert_eq!(stored.bytes, bytes, "bytes must round-trip unchanged");
    assert_eq!(reference.byte_len, bytes.len() as u64);
    assert_eq!(reference.media_type.as_str(), "image/png");
    assert_eq!(
        reference.type_metadata,
        Some(AttachmentTypeMetadata::image(Some(7), Some(11)))
    );
    assert_eq!(reference.label.as_deref(), Some("pixel"));
}

async fn attachment_is_content_addressed(store: Arc<dyn AttachmentStore>) {
    let first = store
        .put(vec![9u8, 9, 9], attachment_meta())
        .await
        .expect("put first");
    let same = store
        .put(vec![9u8, 9, 9], attachment_meta())
        .await
        .expect("put identical bytes");
    let different = store
        .put(vec![9u8, 9, 8], attachment_meta())
        .await
        .expect("put different bytes");

    assert_eq!(
        first.id, same.id,
        "identical bytes must map to the same content-addressed id"
    );
    assert_ne!(
        first.id, different.id,
        "different bytes must map to different ids"
    );
}

async fn attachment_head_reflects_put_and_refreshes_timestamp(store: Arc<dyn AttachmentStore>) {
    let bytes = vec![3u8, 1, 4, 1, 5];
    let reference = store
        .put(bytes.clone(), attachment_meta())
        .await
        .expect("put attachment before head");
    let first_head = store
        .head(&reference.id)
        .await
        .expect("head just-written attachment")
        .expect("head must find a just-written attachment");
    assert_eq!(
        first_head.id, reference.id,
        "head must return the requested just-written attachment"
    );

    let Some(first_modified) = first_head.last_modified_epoch_ms else {
        // `StoredBlobRef` permits timestamp-free backends. They have no write-
        // grace protection, but `head` must still prove presence and identity.
        let repeated = store
            .put(bytes, attachment_meta())
            .await
            .expect("repeat identical put");
        assert_eq!(repeated.id, reference.id);
        let refreshed_head = store
            .head(&reference.id)
            .await
            .expect("head attachment after repeated put")
            .expect("head must still find an attachment after a repeated put");
        assert_eq!(refreshed_head.id, reference.id);
        assert!(
            refreshed_head.last_modified_epoch_ms.is_none(),
            "head timestamp availability must remain stable across a repeated put"
        );
        return;
    };

    // S3 exposes `HEAD` freshness at whole-second HTTP-date resolution while
    // local stores are finer grained. Cross that boundary before the repeated
    // put, then require the very next head to observe the restamp: GC cannot
    // wait for a stale metadata cache to expire.
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let repeated = store
        .put(bytes, attachment_meta())
        .await
        .expect("repeat identical put");
    assert_eq!(
        repeated.id, reference.id,
        "the repeated put must address the same blob"
    );
    let refreshed_head = store
        .head(&reference.id)
        .await
        .expect("head attachment after repeated put")
        .expect("head must still find an attachment after a repeated put");
    assert_eq!(
        refreshed_head.id, reference.id,
        "head must return the requested attachment after a repeated put"
    );
    let refreshed_modified = refreshed_head
        .last_modified_epoch_ms
        .expect("head timestamp availability must remain stable across a repeated put");
    assert!(
        refreshed_modified > first_modified,
        "a repeated identical put must refresh head's reported timestamp: {refreshed_modified} <= {first_modified}"
    );
}

async fn attachment_get_unknown_is_not_found(store: Arc<dyn AttachmentStore>) {
    let err = store
        .get(&AttachmentId::parse("sha256:does-not-exist").expect("valid attachment id"))
        .await
        .expect_err("get of an unknown id must fail");
    assert!(
        matches!(err, AttachmentStoreError::NotFound(_)),
        "unknown id must map to NotFound, got {err:?}"
    );
}

async fn attachment_delete_removes_content_and_is_idempotent(store: Arc<dyn AttachmentStore>) {
    let reference = store
        .put(vec![5u8, 6, 7, 8], attachment_meta())
        .await
        .expect("put attachment to delete");
    // Present before delete.
    store
        .get(&reference.id)
        .await
        .expect("content present before delete");

    store.delete(&reference.id).await.expect("delete content");
    let err = store
        .get(&reference.id)
        .await
        .expect_err("content must be gone after delete");
    assert!(
        matches!(err, AttachmentStoreError::NotFound(_)),
        "deleted content must map to NotFound, got {err:?}"
    );

    // Idempotent: deleting absent content is not an error.
    store
        .delete(&reference.id)
        .await
        .expect("delete of already-absent content is a no-op");
    store
        .delete(&AttachmentId::parse("sha256:never-existed").expect("valid attachment id"))
        .await
        .expect("delete of unknown id is a no-op");
}

async fn attachment_head_reports_absence(store: Arc<dyn AttachmentStore>) {
    let never_written =
        AttachmentId::parse("sha256:never-written-head").expect("valid attachment id");
    assert!(
        store
            .head(&never_written)
            .await
            .expect("head never-written attachment")
            .is_none(),
        "head must report None for a never-written attachment"
    );

    let reference = store
        .put(vec![2u8, 7, 1, 8], attachment_meta())
        .await
        .expect("put attachment before delete");
    store
        .head(&reference.id)
        .await
        .expect("head attachment before delete")
        .expect("head must find attachment before delete");
    store
        .delete(&reference.id)
        .await
        .expect("delete attachment before head");
    assert!(
        store
            .head(&reference.id)
            .await
            .expect("head deleted attachment")
            .is_none(),
        "head must report None for a deleted attachment"
    );
}

async fn attachment_list_enumerates_stored_blobs(store: Arc<dyn AttachmentStore>) {
    // An empty store lists nothing.
    assert!(
        store.list().await.expect("list empty store").is_empty(),
        "a fresh store must enumerate no blobs"
    );

    let first = store
        .put(vec![1u8, 1, 1], attachment_meta())
        .await
        .expect("put first");
    let second = store
        .put(vec![2u8, 2, 2, 2], attachment_meta())
        .await
        .expect("put second");
    // Idempotent duplicate put does not create a second listing.
    store
        .put(vec![1u8, 1, 1], attachment_meta())
        .await
        .expect("put duplicate");

    let listed: std::collections::BTreeSet<AttachmentId> = store
        .list()
        .await
        .expect("list populated store")
        .into_iter()
        .map(|blob| blob.id)
        .collect();
    assert!(listed.contains(&first.id), "list must include first blob");
    assert!(listed.contains(&second.id), "list must include second blob");
    assert_eq!(
        listed.len(),
        2,
        "content-addressed dedup: one entry per blob"
    );

    // A deleted blob leaves the listing.
    store.delete(&first.id).await.expect("delete first");
    let after: std::collections::BTreeSet<AttachmentId> = store
        .list()
        .await
        .expect("list after delete")
        .into_iter()
        .map(|blob| blob.id)
        .collect();
    assert!(
        !after.contains(&first.id),
        "deleted blob must not be listed"
    );
    assert!(after.contains(&second.id), "surviving blob stays listed");
}

async fn attachment_head_agrees_with_list(store: Arc<dyn AttachmentStore>) {
    store
        .put(vec![6u8, 2, 6, 4], attachment_meta())
        .await
        .expect("put first attachment before comparing head and list");
    store
        .put(vec![3u8, 3, 8, 3], attachment_meta())
        .await
        .expect("put second attachment before comparing head and list");

    let listed = store.list().await.expect("list attachments for head check");
    assert_eq!(
        listed.len(),
        2,
        "list must expose both blobs used by the head agreement law"
    );
    for listed_blob in listed {
        let id = listed_blob.id.clone();
        let head = store
            .head(&id)
            .await
            .expect("head listed attachment")
            .expect("head must find every attachment returned by list");
        assert_eq!(
            head.id, id,
            "head must return the attachment requested from list"
        );
        assert_eq!(
            head.last_modified_epoch_ms.is_some(),
            listed_blob.last_modified_epoch_ms.is_some(),
            "head and list must agree on timestamp availability for stored attachment {id}"
        );
    }
}

fn attachment_reports_declared_persistence(
    store: Arc<dyn AttachmentStore>,
    expected: AttachmentStorePersistence,
) {
    assert_eq!(
        store.persistence(),
        expected,
        "persistence tier must match the backend's declared durability"
    );
}

async fn attachment_store_survives_reopen(factory: ReopenableAttachmentStore) {
    let reference = factory
        .open
        .put(vec![4u8, 3, 2, 1], attachment_meta())
        .await
        .expect("put attachment before reopen");
    let reopened = factory
        .reopen
        .get(&reference.id)
        .await
        .expect("get attachment after reopen");
    assert_eq!(reopened.bytes, vec![4u8, 3, 2, 1]);
    assert_eq!(reference.byte_len, 4);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Clone, Copy)]
    enum HeadDefect {
        StaleTimestamp,
        MissingLive,
        PhantomUnknown,
        RetainDeleted,
        MissingTimestamp,
    }

    struct BrokenHeadStore {
        inner: crate::InMemoryAttachmentStore,
        defect: HeadDefect,
        cached: Mutex<BTreeMap<AttachmentId, crate::StoredBlobRef>>,
    }

    struct TtlCachedHeadState {
        cached: Option<crate::StoredBlobRef>,
        stale_until: Option<std::time::Instant>,
    }

    struct TtlCachedHeadStore {
        inner: crate::InMemoryAttachmentStore,
        state: Mutex<TtlCachedHeadState>,
    }

    impl TtlCachedHeadStore {
        fn new() -> Self {
            Self {
                inner: crate::InMemoryAttachmentStore::new(),
                state: Mutex::new(TtlCachedHeadState {
                    cached: None,
                    stale_until: None,
                }),
            }
        }
    }

    impl BrokenHeadStore {
        fn new(defect: HeadDefect) -> Self {
            Self {
                inner: crate::InMemoryAttachmentStore::new(),
                defect,
                cached: Mutex::new(BTreeMap::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl AttachmentStore for BrokenHeadStore {
        async fn put(
            &self,
            bytes: Vec<u8>,
            meta: AttachmentCreateMeta,
        ) -> Result<crate::AttachmentRef, AttachmentStoreError> {
            self.inner.put(bytes, meta).await
        }

        async fn get(
            &self,
            id: &AttachmentId,
        ) -> Result<crate::StoredAttachment, AttachmentStoreError> {
            self.inner.get(id).await
        }

        async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
            self.inner.delete(id).await
        }

        async fn list(&self) -> Result<Vec<crate::StoredBlobRef>, AttachmentStoreError> {
            self.inner.list().await
        }

        async fn head(
            &self,
            id: &AttachmentId,
        ) -> Result<Option<crate::StoredBlobRef>, AttachmentStoreError> {
            let honest = self.inner.head(id).await?;
            match self.defect {
                HeadDefect::StaleTimestamp => Ok(honest.map(|mut blob| {
                    blob.last_modified_epoch_ms = Some(1);
                    blob
                })),
                HeadDefect::MissingLive => Ok(None),
                HeadDefect::PhantomUnknown => {
                    Ok(Some(honest.unwrap_or_else(|| crate::StoredBlobRef {
                        id: id.clone(),
                        last_modified_epoch_ms: None,
                    })))
                }
                HeadDefect::RetainDeleted => match honest {
                    Some(blob) => {
                        self.cached
                            .lock()
                            .expect("head cache lock")
                            .insert(id.clone(), blob.clone());
                        Ok(Some(blob))
                    }
                    None => Ok(self
                        .cached
                        .lock()
                        .expect("head cache lock")
                        .get(id)
                        .cloned()),
                },
                HeadDefect::MissingTimestamp => Ok(honest.map(|mut blob| {
                    blob.last_modified_epoch_ms = None;
                    blob
                })),
            }
        }
    }

    #[async_trait::async_trait]
    impl AttachmentStore for TtlCachedHeadStore {
        async fn put(
            &self,
            bytes: Vec<u8>,
            meta: AttachmentCreateMeta,
        ) -> Result<crate::AttachmentRef, AttachmentStoreError> {
            let reference = self.inner.put(bytes, meta).await?;
            let mut state = self.state.lock().expect("head cache lock");
            if state.cached.is_some() && state.stale_until.is_none() {
                state.stale_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
            }
            Ok(reference)
        }

        async fn get(
            &self,
            id: &AttachmentId,
        ) -> Result<crate::StoredAttachment, AttachmentStoreError> {
            self.inner.get(id).await
        }

        async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
            self.inner.delete(id).await
        }

        async fn list(&self) -> Result<Vec<crate::StoredBlobRef>, AttachmentStoreError> {
            self.inner.list().await
        }

        async fn head(
            &self,
            id: &AttachmentId,
        ) -> Result<Option<crate::StoredBlobRef>, AttachmentStoreError> {
            let honest = self.inner.head(id).await?;
            let mut state = self.state.lock().expect("head cache lock");
            if state
                .stale_until
                .is_some_and(|deadline| std::time::Instant::now() < deadline)
            {
                return Ok(state.cached.clone());
            }
            state.cached = honest.clone();
            Ok(honest)
        }
    }

    #[tokio::test]
    #[should_panic(expected = "a repeated identical put must refresh head's reported timestamp")]
    async fn head_freshness_law_rejects_a_stale_timestamp() {
        attachment_store(
            || {
                Arc::new(BrokenHeadStore::new(HeadDefect::StaleTimestamp))
                    as Arc<dyn AttachmentStore>
            },
            AttachmentStorePersistence::Ephemeral,
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "a repeated identical put must refresh head's reported timestamp")]
    async fn head_freshness_law_rejects_a_transiently_cached_timestamp() {
        attachment_store(
            || Arc::new(TtlCachedHeadStore::new()) as Arc<dyn AttachmentStore>,
            AttachmentStorePersistence::Ephemeral,
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "head must find a just-written attachment")]
    async fn head_freshness_law_rejects_a_missing_live_blob() {
        attachment_store(
            || Arc::new(BrokenHeadStore::new(HeadDefect::MissingLive)) as Arc<dyn AttachmentStore>,
            AttachmentStorePersistence::Ephemeral,
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "head must report None for a never-written attachment")]
    async fn head_absence_law_rejects_a_phantom_unknown_blob() {
        attachment_store(
            || {
                Arc::new(BrokenHeadStore::new(HeadDefect::PhantomUnknown))
                    as Arc<dyn AttachmentStore>
            },
            AttachmentStorePersistence::Ephemeral,
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "head must report None for a deleted attachment")]
    async fn head_absence_law_rejects_a_deleted_blob_that_remains_visible() {
        attachment_store(
            || {
                Arc::new(BrokenHeadStore::new(HeadDefect::RetainDeleted))
                    as Arc<dyn AttachmentStore>
            },
            AttachmentStorePersistence::Ephemeral,
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "head and list must agree on timestamp availability")]
    async fn head_list_agreement_law_rejects_missing_freshness() {
        attachment_store(
            || {
                Arc::new(BrokenHeadStore::new(HeadDefect::MissingTimestamp))
                    as Arc<dyn AttachmentStore>
            },
            AttachmentStorePersistence::Ephemeral,
        )
        .await;
    }
}
