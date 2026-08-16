use super::*;
use lash_sansio::{AttachmentTypeMetadata, MediaType};

#[derive(Default)]
struct RecordingManifest {
    entries: Mutex<HashMap<(String, AttachmentId), crate::AttachmentManifestEntry>>,
}

impl AttachmentManifest for RecordingManifest {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), crate::StoreError> {
        let key = (intent.session_id.clone(), intent.attachment_id.clone());
        self.entries
            .lock_recover()
            .entry(key)
            .or_insert(crate::AttachmentManifestEntry {
                attachment_id: intent.attachment_id,
                session_id: intent.session_id,
                canonical_uri: intent.canonical_uri,
                intent_at_epoch_ms: intent.intent_at_epoch_ms,
                committed_at_epoch_ms: None,
                owner_kind: intent.owner_kind,
                owner_id: intent.owner_id,
            });
        Ok(())
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), crate::StoreError> {
        let mut entries = self.entries.lock_recover();
        for attachment_id in attachment_ids {
            if let Some(entry) = entries.get_mut(&(session_id.to_string(), attachment_id.clone())) {
                entry.committed_at_epoch_ms.get_or_insert(1);
            }
        }
        Ok(())
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<crate::AttachmentManifestEntry>, crate::StoreError> {
        Ok(self
            .entries
            .lock_recover()
            .values()
            .filter(|entry| {
                entry.committed_at_epoch_ms.is_none()
                    && entry.intent_at_epoch_ms <= older_than_epoch_ms
            })
            .cloned()
            .collect())
    }

    fn forget(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<(), crate::StoreError> {
        self.entries
            .lock_recover()
            .remove(&(session_id.to_string(), attachment_id.clone()));
        Ok(())
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<bool, crate::StoreError> {
        Ok(self
            .entries
            .lock_recover()
            .contains_key(&(session_id.to_string(), attachment_id.clone())))
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, crate::StoreError> {
        Ok(self
            .entries
            .lock_recover()
            .values()
            .map(|entry| entry.attachment_id.clone())
            .collect())
    }
}

/// Root set backed by a set of [`RecordingManifest`]s — the in-memory
/// analogue of a factory unioning its sessions' refs.
struct RecordingRootSet {
    manifests: Vec<Arc<RecordingManifest>>,
}

struct UnavailableRootSet;

#[async_trait::async_trait]
impl AttachmentRootSet for UnavailableRootSet {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, crate::StoreError> {
        Err(crate::StoreError::Backend(
            "root enumeration unavailable".to_string(),
        ))
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Err(crate::StoreError::Backend(
            "targeted root probe unavailable".to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl AttachmentRootSet for RecordingRootSet {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, crate::StoreError> {
        let mut refs = BTreeSet::new();
        for manifest in &self.manifests {
            // This test root set contains only ownerless host puts, whose
            // documented fallback remains age-only reconciliation.
            for aged in manifest.list_uncommitted(intent_grace_cutoff_epoch_ms)? {
                manifest.forget(&aged.session_id, &aged.attachment_id)?;
            }
            refs.extend(manifest.list_all_refs()?);
        }
        Ok(refs)
    }

    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        for manifest in &self.manifests {
            if manifest.has_live_ref_for_id(id, intent_grace_cutoff_epoch_ms)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[tokio::test]
async fn recording_targeted_probe_does_not_reconcile_aged_intent() {
    let manifest = Arc::new(RecordingManifest::default());
    let id = content_id(b"aged-targeted-probe-root");
    manifest
        .record_intent(AttachmentIntent {
            attachment_id: id.clone(),
            session_id: "targeted-probe".to_string(),
            canonical_uri: attachment_uri(&id),
            intent_at_epoch_ms: 1,
            owner_kind: None,
            owner_id: None,
        })
        .expect("record intent");
    let roots = RecordingRootSet {
        manifests: vec![Arc::clone(&manifest)],
    };

    assert!(roots.has_live_attachment_ref(&id, 1).await.unwrap());
    assert_eq!(manifest.list_all_refs().unwrap(), vec![id]);
}

fn meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(
        MediaType::parse("image/png").unwrap(),
        Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
        Some("pixel".to_string()),
    )
}

async fn committed_factory_attachment() -> (
    crate::InMemorySessionStoreFactory,
    Arc<InMemoryAttachmentStore>,
    AttachmentId,
) {
    let factory = crate::InMemorySessionStoreFactory::new();
    let request = crate::SessionStoreCreateRequest {
        session_id: "explicit-root-factory".to_string(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    };
    let store = crate::SessionStoreFactory::create_store(&factory, &request)
        .await
        .expect("create attachment-aware store");
    let backend = Arc::new(InMemoryAttachmentStore::new());
    let session = SessionAttachmentStore::new(
        backend.clone(),
        Arc::new(PersistenceManifestAdapter(Arc::clone(&store))),
        request.session_id.clone(),
    );
    let reference = session
        .put(vec![8, 8, 1], meta())
        .await
        .expect("put factory attachment");
    store
        .commit_refs(&request.session_id, std::slice::from_ref(&reference.id))
        .expect("commit factory attachment ref");
    (factory, backend, reference.id)
}

#[tokio::test]
async fn explicit_factory_root_set_keeps_committed_blob() {
    let (factory, backend, id) = committed_factory_attachment().await;

    let report = reclaim_unreferenced_attachments(
        &factory,
        &*backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("sweep committed factory attachment");

    assert_eq!(report.scanned_blob_count, 1);
    assert_eq!(report.reclaimed_count, 0);
    assert!(report.failed_ids.is_empty());
    assert!(report.deleted_while_referenced.is_empty());
    backend.get(&id).await.expect("committed blob survives");
}

/// Deliberately faulty snapshot projection over a factory that really does hold
/// a committed ref, and whose targeted probe misses too. Every read-shaped guard
/// is therefore blind; only the factory's condemn CAS — the authority the
/// writer's intent lives in — can still see the root.
struct EmptySnapshotFactoryRoots<'a> {
    factory: &'a crate::InMemorySessionStoreFactory,
}

#[async_trait::async_trait]
impl AttachmentRootSet for EmptySnapshotFactoryRoots<'_> {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, crate::StoreError> {
        Ok(BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Ok(false)
    }

    fn fence(&self) -> crate::AttachmentGcFence {
        AttachmentRootSet::fence(self.factory)
    }

    async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<crate::AttachmentCondemnation, crate::StoreError> {
        AttachmentRootSet::condemn_attachment(self.factory, id, intent_grace_cutoff_epoch_ms).await
    }

    async fn arm_attachment_delete(
        &self,
        id: &AttachmentId,
    ) -> Result<crate::AttachmentDeleteArming, crate::StoreError> {
        AttachmentRootSet::arm_attachment_delete(self.factory, id).await
    }

    async fn release_attachment_condemnation(
        &self,
        id: &AttachmentId,
    ) -> Result<(), crate::StoreError> {
        AttachmentRootSet::release_attachment_condemnation(self.factory, id).await
    }
}

/// Survival proof: the condemn CAS is a *conditional mutation* in the authority
/// that owns the roots, not another read. A snapshot and a targeted probe that
/// both miss a committed ref no longer cost the blob its bytes.
#[tokio::test]
async fn condemn_cas_spares_a_live_blob_every_read_shaped_guard_missed() {
    let (factory, backend, id) = committed_factory_attachment().await;
    let roots = EmptySnapshotFactoryRoots { factory: &factory };

    let report = reclaim_unreferenced_attachments(
        &roots,
        &*backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .expect("sweep with deliberately empty snapshot");

    assert_eq!(report.fence, crate::AttachmentGcFence::Fenced);
    assert_eq!(
        report.reclaimed_count, 0,
        "the condemn CAS must refuse a digest whose committed ref the reads missed"
    );
    assert!(report.deleted_while_referenced.is_empty());
    backend
        .get(&id)
        .await
        .expect("the committed blob survives a blind snapshot and a blind probe");
}

#[tokio::test]
async fn memory_store_dedupes_by_bytes() {
    let store = InMemoryAttachmentStore::new();
    let a = store.put(vec![1, 2, 3], meta()).await.expect("put a");
    let b = store.put(vec![1, 2, 3], meta()).await.expect("put b");
    assert_eq!(a.id, b.id);
    assert_eq!(a.byte_len, 3);
    assert_eq!(store.get(&a.id).await.expect("get").bytes, vec![1, 2, 3]);
}

#[tokio::test]
async fn memory_store_assigns_identity_and_byte_len_from_bytes() {
    let store = InMemoryAttachmentStore::new();
    let reference = store.put(vec![4, 5, 6, 7], meta()).await.expect("put");

    assert_eq!(reference.id, content_id(&[4, 5, 6, 7]));
    assert_eq!(reference.byte_len, 4);
}

#[tokio::test]
async fn memory_store_lists_stored_blobs() {
    let store = InMemoryAttachmentStore::new();
    let a = store.put(vec![1], meta()).await.expect("put a");
    let b = store.put(vec![2], meta()).await.expect("put b");
    let listed: BTreeSet<AttachmentId> = store
        .list()
        .await
        .expect("list")
        .into_iter()
        .map(|blob| blob.id)
        .collect();
    assert!(listed.contains(&a.id));
    assert!(listed.contains(&b.id));
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
async fn gc_refuses_an_empty_root_set_with_a_deletion_eligible_blob() {
    let backend = InMemoryAttachmentStore::new();
    let attachment = backend
        .put(vec![4, 2, 4, 6], meta())
        .await
        .expect("put deletion-eligible blob");
    let roots = RecordingRootSet { manifests: vec![] };

    let result = reclaim_unreferenced_attachments(
        &roots,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await;

    let error = result.expect_err("empty roots must refuse deletion");
    assert!(matches!(&error, AttachmentStoreError::EmptyRootSetRefused));
    assert!(
        error.to_string().contains("AuthorizeDeleteAll"),
        "the typed refusal must name the explicit remedy: {error}"
    );
    backend
        .get(&attachment.id)
        .await
        .expect("refused sweep preserves the blob");
}

#[tokio::test]
async fn gc_explicit_authorization_permits_an_empty_root_set_sweep() {
    let backend = InMemoryAttachmentStore::new();
    let attachment = backend
        .put(vec![4, 2, 4, 7], meta())
        .await
        .expect("put deletion-eligible blob");
    let roots = RecordingRootSet { manifests: vec![] };

    let report = reclaim_unreferenced_attachments(
        &roots,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .expect("explicit authorization permits delete-all interpretation");

    assert_eq!(report.reclaimed_count, 1);
    assert!(matches!(
        backend.get(&attachment.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn gc_empty_root_set_does_not_refuse_when_every_blob_is_fresh() {
    let backend = InMemoryAttachmentStore::new();
    let attachment = backend
        .put(vec![4, 2, 4, 8], meta())
        .await
        .expect("put fresh blob");
    let roots = RecordingRootSet { manifests: vec![] };

    let report = reclaim_unreferenced_attachments(
        &roots,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 60 * 60 * 1000,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("fresh-only backend has no deletion candidate");

    assert_eq!(report.reclaimed_count, 0);
    backend
        .get(&attachment.id)
        .await
        .expect("fresh blob survives");
}

#[tokio::test]
async fn gc_reports_root_enumeration_failure_when_every_blob_is_fresh() {
    let backend = InMemoryAttachmentStore::new();
    let attachment = backend
        .put(vec![4, 2, 4, 9], meta())
        .await
        .expect("put fresh blob");

    let report = reclaim_unreferenced_attachments(
        &UnavailableRootSet,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 60 * 60 * 1000,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("fresh-only backend has no deletion candidate");

    assert_eq!(report.scanned_blob_count, 1);
    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(
        report.root_enumeration_failure.as_deref(),
        Some(
            "failed to enumerate live attachment refs: store backend error: root enumeration unavailable"
        )
    );
    backend
        .get(&attachment.id)
        .await
        .expect("fresh blob survives degraded sweep");
}

#[tokio::test]
async fn gc_non_empty_root_set_still_reclaims_an_unreferenced_blob() {
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let manifest = Arc::new(RecordingManifest::default());
    let session = SessionAttachmentStore::new(
        Arc::clone(&backend),
        manifest.clone() as Arc<dyn AttachmentManifest>,
        "healthy-sweep",
    );
    let live = session
        .put(vec![4, 2, 4, 9], meta())
        .await
        .expect("put live blob");
    manifest
        .commit_refs("healthy-sweep", std::slice::from_ref(&live.id))
        .expect("commit live ref");
    let orphan = backend
        .put(vec![4, 2, 5, 0], meta())
        .await
        .expect("put orphan blob");
    let roots = RecordingRootSet {
        manifests: vec![manifest],
    };

    let report = reclaim_unreferenced_attachments(
        &roots,
        &*backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("healthy non-empty-root sweep");

    assert_eq!(report.reclaimed_count, 1);
    backend.get(&live.id).await.expect("live blob survives");
    assert!(matches!(
        backend.get(&orphan.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn facade_get_is_gated_by_manifest_ownership() {
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let manifest: Arc<dyn AttachmentManifest> = Arc::new(RecordingManifest::default());
    let session_a = SessionAttachmentStore::new(backend.clone(), manifest.clone(), "session-a");
    let session_b = SessionAttachmentStore::new(backend.clone(), manifest.clone(), "session-b");

    let reference = session_a.put(vec![7, 7, 7], meta()).await.expect("put a");
    // Session A holds the ref and resolves the blob.
    assert_eq!(
        session_a.get(&reference.id).await.expect("a reads").bytes,
        vec![7, 7, 7]
    );
    // Session B shares the backend blob but never referenced it: NotFound.
    assert!(matches!(
        session_b.get(&reference.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn facade_delete_drops_ref_but_keeps_backend_bytes() {
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let manifest: Arc<dyn AttachmentManifest> = Arc::new(RecordingManifest::default());
    let session = SessionAttachmentStore::new(backend.clone(), manifest, "session-1");

    let reference = session.put(vec![9, 9], meta()).await.expect("put");
    session.delete(&reference.id).await.expect("delete ref");

    // Facade no longer resolves it (ref dropped)...
    assert!(matches!(
        session.get(&reference.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
    // ...but the backend still physically holds the bytes (GC's job).
    assert_eq!(
        backend
            .get(&reference.id)
            .await
            .expect("bytes remain")
            .bytes,
        vec![9, 9]
    );
}

#[tokio::test]
async fn shared_bytes_survive_until_all_refs_released_then_gc_collects() {
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let manifest_a = Arc::new(RecordingManifest::default());
    let manifest_b = Arc::new(RecordingManifest::default());
    let session_a = SessionAttachmentStore::new(
        backend.clone(),
        manifest_a.clone() as Arc<dyn AttachmentManifest>,
        "session-a",
    );
    let session_b = SessionAttachmentStore::new(
        backend.clone(),
        manifest_b.clone() as Arc<dyn AttachmentManifest>,
        "session-b",
    );

    // Two sessions put identical bytes: ONE physical blob. Commit both refs
    // so they are stable committed roots (not grace-gated intents), letting
    // this test exercise ref-driven collection independently of intent aging.
    let ref_a = session_a.put(vec![5, 5, 5], meta()).await.expect("put a");
    let ref_b = session_b.put(vec![5, 5, 5], meta()).await.expect("put b");
    assert_eq!(ref_a.id, ref_b.id);
    manifest_a
        .commit_refs("session-a", std::slice::from_ref(&ref_a.id))
        .expect("commit a");
    manifest_b
        .commit_refs("session-b", std::slice::from_ref(&ref_b.id))
        .expect("commit b");
    assert_eq!(backend.list().await.expect("list").len(), 1);

    let root_set = RecordingRootSet {
        manifests: vec![manifest_a.clone(), manifest_b.clone()],
    };

    // Session A releases its ref. Blob is still referenced by B: spared.
    session_a.delete(&ref_a.id).await.expect("a releases");
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &*backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("sweep with b holding a ref");
    assert_eq!(report.reclaimed_count, 0, "b still references the blob");
    assert_eq!(
        backend.get(&ref_b.id).await.expect("blob alive").bytes,
        vec![5, 5, 5]
    );

    // Session B releases too. Now unreferenced: GC collects it.
    session_b.delete(&ref_b.id).await.expect("b releases");
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &*backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .expect("sweep with no refs");
    assert_eq!(report.reclaimed_count, 1);
    assert!(matches!(
        backend.get(&ref_b.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn gc_spares_fresh_in_flight_intents_as_refs() {
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let manifest = Arc::new(RecordingManifest::default());
    let session = SessionAttachmentStore::new(
        backend.clone(),
        manifest.clone() as Arc<dyn AttachmentManifest>,
        "session-1",
    );

    // Fresh ownerless host intent: the legacy fallback retains it through
    // the grace window.
    let reference = session.put(vec![3, 1, 4], meta()).await.expect("put");
    let root_set = RecordingRootSet {
        manifests: vec![manifest.clone()],
    };
    const GRACE_MS: u64 = 60 * 60 * 1000;
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &*backend,
        AttachmentReclamationPolicy {
            grace_period_ms: GRACE_MS,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("sweep");
    assert_eq!(report.reclaimed_count, 0, "a fresh intent is a live ref");
    assert_eq!(
        backend.get(&reference.id).await.expect("kept").bytes,
        vec![3, 1, 4]
    );
}

// Ownerless host puts have no durable liveness proof, so their fallback is
// age-only: an old intent is reconciled and a fresh one survives.
#[tokio::test]
async fn gc_collects_aged_uncommitted_intent_orphan() {
    let backend: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let manifest = Arc::new(RecordingManifest::default());
    let session = SessionAttachmentStore::new(
        backend.clone(),
        manifest.clone() as Arc<dyn AttachmentManifest>,
        "session-1",
    );

    // With a zero grace window the ownerless intent is already eligible.
    let orphan = session
        .put(vec![9, 9, 9], meta())
        .await
        .expect("put orphan");
    let root_set = RecordingRootSet {
        manifests: vec![manifest.clone()],
    };
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &*backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .expect("sweep");
    assert_eq!(
        report.reclaimed_count, 1,
        "an aged, never-committed intent is a collectable orphan"
    );
    assert!(matches!(
        backend.get(&orphan.id).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
    // The intent row was reconciled away too, so it is no longer a root.
    assert!(manifest.list_all_refs().expect("refs").is_empty());
}

// Fix C: the GC delete-time re-check. A blob looks unreferenced and stale in
// the `list` snapshot, but a new intent + `put` of the same content id landed
// after the snapshot, refreshing the blob. The sweep must re-stat the blob via
// `head` before deleting and spare it. Modelled sequentially with a backend
// whose `list` reports a stale mtime and whose `head` reports a fresh one.
#[tokio::test]
async fn gc_delete_recheck_spares_blob_refreshed_after_snapshot() {
    struct StaleSnapshotStore {
        id: AttachmentId,
        list_mtime: u64,
        head_mtime: u64,
        deleted: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl AttachmentStore for StaleSnapshotStore {
        async fn put(
            &self,
            _bytes: Vec<u8>,
            _meta: AttachmentCreateMeta,
        ) -> Result<AttachmentRef, AttachmentStoreError> {
            unreachable!("test does not put through this store")
        }
        async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
            Err(AttachmentStoreError::NotFound(id.clone()))
        }
        async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
            *self.deleted.lock_recover() = true;
            Ok(())
        }
        async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
            // Stale snapshot: the blob looks old and unreferenced.
            Ok(vec![StoredBlobRef {
                id: self.id.clone(),
                last_modified_epoch_ms: Some(self.list_mtime),
            }])
        }
        async fn head(
            &self,
            id: &AttachmentId,
        ) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
            // Fresh re-stat: the new intent's `put` refreshed the blob.
            Ok((id == &self.id).then(|| StoredBlobRef {
                id: id.clone(),
                last_modified_epoch_ms: Some(self.head_mtime),
            }))
        }
    }

    let now = now_epoch_ms();
    const GRACE_MS: u64 = 60 * 60 * 1000;
    let backend = StaleSnapshotStore {
        id: AttachmentId::new("recheck"),
        // Stale mtime well past the grace window: the first check would delete.
        list_mtime: now.saturating_sub(GRACE_MS * 2),
        // Fresh mtime inside the window: the re-check must spare it.
        head_mtime: now,
        deleted: Mutex::new(false),
    };
    // Empty root set: the snapshot did not see the new intent.
    let root_set = RecordingRootSet { manifests: vec![] };
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: GRACE_MS,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("sweep");
    assert_eq!(
        report.reclaimed_count, 0,
        "the delete-time re-check must spare a blob refreshed after the snapshot"
    );
    assert!(
        !*backend.deleted.lock_recover(),
        "the freshly-refreshed blob must not be deleted"
    );
}

// Fix C, checks (b) and (d): the delete-window root re-check. A candidate blob
// is stale in both the `list` snapshot AND the `head` re-stat (so the freshness
// gate does not spare it), but a session records a fresh intent for the same
// content id in the delete window. A backend whose `head` reports a stale mtime
// and a root set scripted to answer the single-id probe drive the two branches:
// (b) a ref present before delete spares the blob; (d) a ref that appears only
// after delete is detected and alarmed via `deleted_while_referenced`.
struct StaleHeadStore {
    id: AttachmentId,
    mtime: u64,
    deleted: Mutex<bool>,
}

#[async_trait::async_trait]
impl AttachmentStore for StaleHeadStore {
    async fn put(
        &self,
        _bytes: Vec<u8>,
        _meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        unreachable!("test does not put through this store")
    }
    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        Err(AttachmentStoreError::NotFound(id.clone()))
    }
    async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        *self.deleted.lock_recover() = true;
        Ok(())
    }
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        Ok(vec![StoredBlobRef {
            id: self.id.clone(),
            last_modified_epoch_ms: Some(self.mtime),
        }])
    }
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        // Stale re-stat: the freshness gate does not spare the blob, so the
        // targeted root re-check is what must decide its fate.
        Ok((id == &self.id).then(|| StoredBlobRef {
            id: id.clone(),
            last_modified_epoch_ms: Some(self.mtime),
        }))
    }
}

/// Root set whose single-id probe returns scripted answers in order — models
/// a ref appearing at a chosen point in the delete window. `live_attachment_refs`
/// is empty (the snapshot never saw the late ref).
struct ScriptedRootSet {
    answers: Mutex<std::collections::VecDeque<bool>>,
}

#[async_trait::async_trait]
impl AttachmentRootSet for ScriptedRootSet {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, crate::StoreError> {
        Ok(BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Ok(self.answers.lock_recover().pop_front().unwrap_or(false))
    }
}

#[tokio::test]
async fn gc_pre_delete_root_recheck_spares_reappeared_ref() {
    let now = now_epoch_ms();
    const GRACE_MS: u64 = 60 * 60 * 1000;
    let backend = StaleHeadStore {
        id: AttachmentId::new("reappeared"),
        // Well past the grace window: neither the snapshot nor the head re-stat
        // spares it, so the single-id root re-check is the only guard left.
        mtime: now.saturating_sub(GRACE_MS * 2),
        deleted: Mutex::new(false),
    };
    // (b) sees a live ref: the probe answers true before the delete.
    let root_set = ScriptedRootSet {
        answers: Mutex::new([true].into_iter().collect()),
    };
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: GRACE_MS,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("sweep");
    assert_eq!(
        report.reclaimed_count, 0,
        "pre-delete root re-check must spare the blob"
    );
    assert!(report.deleted_while_referenced.is_empty());
    assert!(
        !*backend.deleted.lock_recover(),
        "a blob re-referenced before delete must not be deleted"
    );
}

// An unfenced/legacy root authority — one that implements neither the
// condemnation transitions nor the writer's fenced intent insert — cannot close
// the delete window. The operation must say so rather than imply a guarantee:
// it reports `BestEffort`, and a ref that appears in the window is recorded as
// detection telemetry (the bytes are gone; lash cannot restore them, which is
// why such deployments belong on a backend with recoverable deletion).
#[tokio::test]
async fn unfenced_root_authority_reports_best_effort_and_detects_the_window_loss() {
    let now = now_epoch_ms();
    const GRACE_MS: u64 = 60 * 60 * 1000;
    let id = AttachmentId::new("window-ref");
    let backend = StaleHeadStore {
        id: id.clone(),
        mtime: now.saturating_sub(GRACE_MS * 2),
        deleted: Mutex::new(false),
    };
    // The pre-delete probe sees no ref (delete proceeds); the post-delete probe
    // sees a ref that appeared in the window.
    let root_set = ScriptedRootSet {
        answers: Mutex::new([false, true].into_iter().collect()),
    };
    let report = reclaim_unreferenced_attachments(
        &root_set,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: GRACE_MS,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .expect("sweep");
    assert_eq!(
        report.fence,
        crate::AttachmentGcFence::BestEffort,
        "an authority with no condemnation CAS must report itself best-effort"
    );
    assert_eq!(report.reclaimed_count, 1, "the blob is deleted");
    assert!(*backend.deleted.lock_recover(), "delete happened");
    assert_eq!(
        report.deleted_while_referenced,
        vec![id],
        "the unfenced path detects the window loss it cannot prevent"
    );
}

// ---------------------------------------------------------------------------
// The GC fence: a real writer against a real fenced root authority.
//
// The in-memory factory owns both halves the fence needs — the manifest the
// writer records intents in and the per-digest condemnation state the sweep
// CASes — so these exercise the same protocol a durable deployment runs, with
// the sweep's own backend calls as the interleaving points.
// ---------------------------------------------------------------------------

type WindowHook =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// Backend wrapper that runs a hook the first time the sweep calls `head` (the
/// digest is condemned but no delete is issued) or `delete` (the delete is in
/// flight) — the two instants a concurrent same-content write can land in.
struct WindowHookedStore {
    inner: Arc<InMemoryAttachmentStore>,
    on_head: Mutex<Option<WindowHook>>,
    on_delete: Mutex<Option<WindowHook>>,
    delete_calls: Mutex<usize>,
}

impl WindowHookedStore {
    fn new(inner: Arc<InMemoryAttachmentStore>) -> Self {
        Self {
            inner,
            on_head: Mutex::new(None),
            on_delete: Mutex::new(None),
            delete_calls: Mutex::new(0),
        }
    }

    async fn fire(hook: &Mutex<Option<WindowHook>>) {
        let taken = hook.lock_recover().take();
        if let Some(hook) = taken {
            hook().await;
        }
    }
}

#[async_trait::async_trait]
impl AttachmentStore for WindowHookedStore {
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }
    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.inner.get(id).await
    }
    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        *self.delete_calls.lock_recover() += 1;
        Self::fire(&self.on_delete).await;
        self.inner.delete(id).await
    }
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        Self::fire(&self.on_head).await;
        self.inner.head(id).await
    }
}

struct FencedFixture {
    factory: crate::InMemorySessionStoreFactory,
    store: Arc<dyn crate::RuntimePersistence>,
    backend: Arc<InMemoryAttachmentStore>,
    session: Arc<SessionAttachmentStore>,
    session_id: String,
    /// Every fence outcome the facade observed, in order.
    fence_attempts:
        Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<AttachmentWriteFence>>>,
}

async fn fenced_fixture(session_id: &str) -> FencedFixture {
    let factory = crate::InMemorySessionStoreFactory::new();
    let request = crate::SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    };
    let store = crate::SessionStoreFactory::create_store(&factory, &request)
        .await
        .expect("create attachment-aware store");
    let backend = Arc::new(InMemoryAttachmentStore::new());
    let (attempts, fence_attempts) = tokio::sync::mpsc::unbounded_channel();
    let session = Arc::new(SessionAttachmentStore::new(
        Arc::clone(&backend) as Arc<dyn AttachmentStore>,
        Arc::new(SignalingManifest {
            inner: Arc::new(PersistenceManifestAdapter(Arc::clone(&store))),
            attempts,
        }),
        session_id.to_string(),
    ));
    FencedFixture {
        factory,
        store,
        backend,
        session,
        session_id: session_id.to_string(),
        fence_attempts: Arc::new(tokio::sync::Mutex::new(fence_attempts)),
    }
}

fn collecting_policy() -> AttachmentReclamationPolicy {
    AttachmentReclamationPolicy {
        grace_period_ms: 0,
        empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
    }
}

/// Spawn a facade `put` from inside a sweep hook and hand control back only when
/// the writer has actually reached the point the window is about to test: it
/// waits for the writer's first fence outcome, and — when that outcome is a
/// grant — for the bytes to land. No sleeps, no scheduling luck.
fn window_writer(
    session: Arc<SessionAttachmentStore>,
    bytes: Vec<u8>,
    attempts: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<AttachmentWriteFence>>>,
    handle_slot: Arc<Mutex<Option<tokio::task::JoinHandle<Result<AttachmentRef, String>>>>>,
) -> WindowHook {
    Arc::new(move || {
        let session = Arc::clone(&session);
        let bytes = bytes.clone();
        let attempts = Arc::clone(&attempts);
        let handle_slot = Arc::clone(&handle_slot);
        Box::pin(async move {
            let (put_done, put_done_rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let outcome = session
                    .put(bytes, meta())
                    .await
                    .map_err(|err| err.to_string());
                let _ = put_done.send(());
                outcome
            });
            *handle_slot.lock_recover() = Some(handle);
            let mut attempts = attempts.lock().await;
            match attempts.recv().await {
                // The writer is inside the window with an intent recorded: give
                // its bytes time to land before the window closes.
                Some(AttachmentWriteFence::Granted) => {
                    let _ = put_done_rx.await;
                }
                // The writer is parked on the fence. Nothing more will happen
                // until this window closes, which is the point.
                Some(AttachmentWriteFence::ReclamationInFlight) | None => {}
            }
        })
    })
}

/// Manifest wrapper that reports each fence acquisition the facade makes, so a
/// window hook can wait for the writer instead of racing it.
struct SignalingManifest {
    inner: Arc<dyn AttachmentManifest>,
    attempts: tokio::sync::mpsc::UnboundedSender<AttachmentWriteFence>,
}

impl AttachmentManifest for SignalingManifest {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), crate::StoreError> {
        self.inner.record_intent(intent)
    }

    fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<AttachmentWriteFence, crate::StoreError> {
        let fence = self.inner.begin_attachment_write(intent)?;
        let _ = self.attempts.send(fence);
        Ok(fence)
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), crate::StoreError> {
        self.inner.commit_refs(session_id, attachment_ids)
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<crate::AttachmentManifestEntry>, crate::StoreError> {
        self.inner.list_uncommitted(older_than_epoch_ms)
    }

    fn forget(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<(), crate::StoreError> {
        self.inner.forget(session_id, attachment_id)
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<bool, crate::StoreError> {
        self.inner.holds_ref(session_id, attachment_id)
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, crate::StoreError> {
        self.inner.list_all_refs()
    }
}

/// SURVIVAL PROOF (the delete window). A session writes the same content while
/// the sweep's physical delete is already in flight — the exact interleaving the
/// pre-fence sweep reported as `deleted_while_referenced` after the bytes were
/// gone. The writer cannot record an intent for an armed digest, so it parks,
/// and the release that follows the delete lets it record and re-put: the
/// content is present when the sweep returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_content_put_inside_the_delete_window_survives() {
    let fixture = fenced_fixture("delete-window-writer").await;
    let bytes = vec![7, 1, 7];
    let id = content_id(&bytes);
    // Genuine garbage: identical bytes with no live root, so the sweep is right
    // to collect them.
    fixture
        .backend
        .put(bytes.clone(), meta())
        .await
        .expect("seed the unreferenced blob");

    let handle_slot = Arc::new(Mutex::new(None));
    let backend = WindowHookedStore::new(Arc::clone(&fixture.backend));
    *backend.on_delete.lock_recover() = Some(window_writer(
        Arc::clone(&fixture.session),
        bytes.clone(),
        Arc::clone(&fixture.fence_attempts),
        Arc::clone(&handle_slot),
    ));

    let report = reclaim_unreferenced_attachments(&fixture.factory, &backend, collecting_policy())
        .await
        .expect("sweep");

    let reference = handle_slot
        .lock_recover()
        .take()
        .expect("the window writer was started")
        .await
        .expect("writer task")
        .expect("the write completes rather than failing on the fence");
    assert_eq!(reference.id, id);
    assert_eq!(
        fixture
            .backend
            .get(&id)
            .await
            .expect("the write that landed in the delete window survives")
            .bytes,
        bytes
    );
    assert!(
        crate::AttachmentManifest::holds_ref(&*fixture.store, &fixture.session_id, &id)
            .expect("manifest probe"),
        "the surviving bytes are rooted by the writer's intent"
    );
    assert!(
        report.deleted_while_referenced.is_empty(),
        "no root exists for an armed digest, so the detector must stay silent"
    );
    assert_eq!(report.fence, crate::AttachmentGcFence::Fenced);
}

/// CONTENTION (the condemned window). The sweep has condemned the digest but not
/// yet armed the delete when a writer takes it back. The arm CAS loses, so no
/// delete is issued at all and the digest is deferred to the next sweep — the
/// sweep never waits for the writer and the writer never waits for the sweep.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_revoking_a_condemnation_defers_the_digest_without_deleting() {
    let fixture = fenced_fixture("condemned-window-writer").await;
    let bytes = vec![2, 7, 1, 8];
    let id = content_id(&bytes);
    fixture
        .backend
        .put(bytes.clone(), meta())
        .await
        .expect("seed the unreferenced blob");

    let handle_slot = Arc::new(Mutex::new(None));
    let backend = WindowHookedStore::new(Arc::clone(&fixture.backend));
    *backend.on_head.lock_recover() = Some(window_writer(
        Arc::clone(&fixture.session),
        bytes.clone(),
        Arc::clone(&fixture.fence_attempts),
        Arc::clone(&handle_slot),
    ));

    let report = reclaim_unreferenced_attachments(&fixture.factory, &backend, collecting_policy())
        .await
        .expect("sweep");

    handle_slot
        .lock_recover()
        .take()
        .expect("the window writer was started")
        .await
        .expect("writer task")
        .expect("a writer in the condemned window is granted immediately");
    assert_eq!(
        report.reclaimed_count, 0,
        "a revoked condemnation must not produce a delete"
    );
    assert_eq!(
        *backend.delete_calls.lock_recover(),
        0,
        "the physical delete is only ever issued for an armed digest"
    );
    assert_eq!(report.condemn_deferred_ids, vec![id.clone()]);
    assert_eq!(
        fixture.backend.get(&id).await.expect("survives").bytes,
        bytes
    );
}

/// SKIP-ON-CONTENTION between sweepers: a digest another sweeper already holds
/// is deferred, not waited on, and no delete is issued for it.
#[tokio::test]
async fn a_peer_sweepers_condemnation_defers_the_digest() {
    let fixture = fenced_fixture("peer-sweeper").await;
    let bytes = vec![3, 3, 3];
    let id = content_id(&bytes);
    fixture
        .backend
        .put(bytes.clone(), meta())
        .await
        .expect("seed the unreferenced blob");

    // A peer sweeper's condemnation.
    assert_eq!(
        AttachmentRootSet::condemn_attachment(&fixture.factory, &id, 0)
            .await
            .expect("first condemn"),
        crate::AttachmentCondemnation::Condemned
    );
    assert_eq!(
        AttachmentRootSet::condemn_attachment(&fixture.factory, &id, 0)
            .await
            .expect("second condemn"),
        crate::AttachmentCondemnation::AlreadyCondemned,
        "a condemned digest is never contended for"
    );

    let backend = WindowHookedStore::new(Arc::clone(&fixture.backend));
    let report = reclaim_unreferenced_attachments(&fixture.factory, &backend, collecting_policy())
        .await
        .expect("sweep");
    assert_eq!(report.condemn_deferred_ids, vec![id.clone()]);
    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(*backend.delete_calls.lock_recover(), 0);
    fixture
        .backend
        .get(&id)
        .await
        .expect("a deferred digest keeps its bytes");

    // The host-owned lever clears a condemnation a dead sweeper left behind; the
    // next sweep then collects the digest normally.
    AttachmentRootSet::release_attachment_condemnation(&fixture.factory, &id)
        .await
        .expect("release");
    let report = reclaim_unreferenced_attachments(&fixture.factory, &backend, collecting_policy())
        .await
        .expect("second sweep");
    assert_eq!(report.reclaimed_count, 1);
    assert!(report.condemn_deferred_ids.is_empty());
}

/// A stuck intent is a root: a turn-owned intent whose turn never commits keeps
/// the blob, and the condemn CAS is what refuses — no age, no clock.
#[tokio::test]
async fn a_stuck_intent_retains_the_blob() {
    let fixture = fenced_fixture("stuck-intent").await;
    let binding = fixture.session.bind_turn_scoped("turn-that-never-commits");
    let reference = fixture
        .session
        .put(vec![4, 4], meta())
        .await
        .expect("put under a turn owner");
    drop(binding);

    assert_eq!(
        AttachmentRootSet::condemn_attachment(&fixture.factory, &reference.id, u64::MAX)
            .await
            .expect("condemn"),
        crate::AttachmentCondemnation::RootPresent,
        "an uncommitted intent whose owner was never superseded is a live root"
    );

    let backend = WindowHookedStore::new(Arc::clone(&fixture.backend));
    let report = reclaim_unreferenced_attachments(&fixture.factory, &backend, collecting_policy())
        .await
        .expect("sweep");
    assert_eq!(report.reclaimed_count, 0);
    assert_eq!(*backend.delete_calls.lock_recover(), 0);
    fixture
        .backend
        .get(&reference.id)
        .await
        .expect("the blob a stuck intent roots survives");
}

#[tokio::test]
async fn session_facade_records_bound_owner_on_put() {
    let manifest = Arc::new(RecordingManifest::default());
    let store = Arc::new(SessionAttachmentStore::new(
        Arc::new(InMemoryAttachmentStore::new()),
        manifest.clone(),
        "session-1",
    ));
    let binding = store.bind_turn_scoped("turn-1");

    let reference = store.put(vec![8, 9, 10], meta()).await.expect("put");
    {
        let entries = manifest.entries.lock_recover();
        let entry = entries
            .get(&("session-1".to_string(), reference.id))
            .expect("manifest entry");
        assert_eq!(entry.owner_kind, Some(crate::AttachmentOwnerKind::Turn));
        assert_eq!(entry.owner_id.as_deref(), Some("turn-1"));
    }

    drop(binding);
    let host_reference = store.put(vec![11, 12], meta()).await.expect("host put");
    let entries = manifest.entries.lock_recover();
    let host_entry = entries
        .get(&("session-1".to_string(), host_reference.id))
        .expect("host manifest entry");
    assert_eq!(host_entry.owner_kind, None);
    assert_eq!(host_entry.owner_id, None);
}

#[tokio::test]
async fn nested_owner_binding_restores_the_previous_owner() {
    let manifest = Arc::new(RecordingManifest::default());
    let store = Arc::new(SessionAttachmentStore::new(
        Arc::new(InMemoryAttachmentStore::new()),
        manifest.clone(),
        "session-1",
    ));
    let process_binding = store.bind_process_scoped("process-1");
    let turn_binding = store.bind_turn_scoped("turn-1");

    let turn_ref = store.put(vec![1], meta()).await.expect("turn put");
    drop(turn_binding);
    let process_ref = store.put(vec![2], meta()).await.expect("process put");
    drop(process_binding);
    let host_ref = store.put(vec![3], meta()).await.expect("host put");

    let entries = manifest.entries.lock_recover();
    let turn = entries
        .get(&("session-1".to_string(), turn_ref.id))
        .expect("turn entry");
    assert_eq!(turn.owner_kind, Some(crate::AttachmentOwnerKind::Turn));
    assert_eq!(turn.owner_id.as_deref(), Some("turn-1"));
    let process = entries
        .get(&("session-1".to_string(), process_ref.id))
        .expect("process entry");
    assert_eq!(
        process.owner_kind,
        Some(crate::AttachmentOwnerKind::Process)
    );
    assert_eq!(process.owner_id.as_deref(), Some("process-1"));
    let host = entries
        .get(&("session-1".to_string(), host_ref.id))
        .expect("host entry");
    assert_eq!(host.owner_kind, None);
    assert_eq!(host.owner_id, None);
}

#[tokio::test]
async fn ephemeral_facade_passes_reads_through_without_a_guard() {
    let store = SessionAttachmentStore::in_memory();
    let reference = store.put(vec![1, 2, 3], meta()).await.expect("put");
    assert_eq!(
        store.get(&reference.id).await.expect("get").bytes,
        vec![1, 2, 3]
    );
}

#[test]
fn persistence_manifest_adapter_forwards_holds_ref() {
    let runtime: Arc<dyn crate::RuntimePersistence> = Arc::new(crate::InMemorySessionStore::new());
    let adapter = PersistenceManifestAdapter(runtime);
    let attachment_id = AttachmentId::new("adapter-forwarding");
    let intent = AttachmentIntent {
        attachment_id: attachment_id.clone(),
        session_id: "adapter-session".to_string(),
        canonical_uri: attachment_uri(&attachment_id),
        intent_at_epoch_ms: 10,
        owner_kind: None,
        owner_id: None,
    };
    adapter.record_intent(intent).expect("record intent");
    assert!(
        adapter
            .holds_ref("adapter-session", &attachment_id)
            .expect("holds ref")
    );
    assert!(
        !adapter
            .holds_ref("other-session", &attachment_id)
            .expect("no ref for other session")
    );
    assert_eq!(
        adapter.list_all_refs().expect("list all refs"),
        vec![attachment_id]
    );
}
