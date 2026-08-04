//! [`AttachmentStore`] conformance: content addressing and round trips.

use super::*;

/// Run the full [`AttachmentStore`] conformance suite against the backend
/// produced by `make`. `make` must return a fresh, empty store on each call.
/// `expected_persistence` is the tier this backend declares (`Ephemeral` for
/// in-memory, `Durable` for file/Sqlite-backed).
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
    attachment_get_unknown_is_not_found(make()).await;
    attachment_delete_removes_content_and_is_idempotent(make()).await;
    attachment_list_enumerates_stored_blobs(make()).await;
    attachment_reports_declared_persistence(make(), expected_persistence);
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

/// Hold a namespaced backend to the [`AttachmentStore`] malformed-id contract.
///
/// This is separate from [`attachment_store`] because pure in-memory stores do
/// not derive storage paths or object keys from ids and therefore have no
/// namespace boundary to enforce.
#[cfg(test)]
pub(crate) async fn attachment_store_rejects_malformed_ids(store: Arc<dyn AttachmentStore>) {
    let malformed = AttachmentId::new("../outside");

    let get_error = store
        .get(&malformed)
        .await
        .expect_err("malformed attachment get must fail");
    let delete_error = store
        .delete(&malformed)
        .await
        .expect_err("malformed attachment delete must fail");
    let head_error = store
        .head(&malformed)
        .await
        .expect_err("malformed attachment head must fail");

    for error in [get_error, delete_error, head_error] {
        assert!(
            matches!(error, AttachmentStoreError::NotFound(ref rejected) if rejected == &malformed),
            "malformed attachment id must map to NotFound, got {error:?}"
        );
    }
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

async fn attachment_get_unknown_is_not_found(store: Arc<dyn AttachmentStore>) {
    let err = store
        .get(&AttachmentId::new("sha256:does-not-exist"))
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
        .delete(&AttachmentId::new("sha256:never-existed"))
        .await
        .expect("delete of unknown id is a no-op");
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
