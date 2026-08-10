use super::*;
use crate::attachments::RAW_ARTIFACT_NAMESPACE;
use lashlang::LashlangArtifactStore;

fn assert_corrupt<T>(result: Result<T, StoreError>, expected_kind: &'static str) {
    match result {
        Err(StoreError::StoredDataCorrupt { record_kind, .. }) => {
            assert_eq!(record_kind, expected_kind);
        }
        _ => panic!("expected StoredDataCorrupt for {expected_kind}"),
    }
}

fn assert_storage_failure<T>(label: &str, result: Result<T, StoreError>) {
    assert!(
        matches!(
            result,
            Err(StoreError::StorageFailure {
                backend: "sqlite",
                ..
            })
        ),
        "expected SQLite StorageFailure from {label}"
    );
}

#[tokio::test]
async fn absent_rows_remain_honest_successful_outcomes() {
    let store = Store::memory().await.expect("open store");
    store.bind_session("absent").expect("bind store");
    assert!(
        store
            .load_session_meta()
            .await
            .expect("read metadata")
            .is_none()
    );
    assert!(
        store
            .load_session_head_meta()
            .await
            .expect("read head")
            .is_none()
    );
    assert!(
        store
            .load_picker_info()
            .await
            .expect("read picker")
            .is_none()
    );
    assert!(
        store
            .load_session_graph()
            .await
            .expect("read graph")
            .nodes
            .is_empty()
    );
    assert!(
        store
            .get_blob(&BlobRef("absent".to_string()))
            .await
            .expect("read blob")
            .is_none()
    );
    assert!(
        store
            .get_checkpoint(&BlobRef("absent".to_string()))
            .await
            .expect("read checkpoint")
            .is_none()
    );
    assert!(
        store
            .load_usage_deltas()
            .await
            .expect("read usage")
            .is_empty()
    );
    assert!(
        lash_core::AttachmentManifest::list_uncommitted(&store, 0)
            .expect("list uncommitted attachments")
            .is_empty()
    );
    assert!(
        lash_core::AttachmentManifest::list_all_refs(&store)
            .expect("list attachment refs")
            .is_empty()
    );

    let raw_bytes = b"legacy non-enveloped blob";
    let raw_blob_ref = store.put_blob(raw_bytes).await;
    assert_eq!(
        store.get_blob(&raw_blob_ref).await.expect("read raw blob"),
        Some(raw_bytes.to_vec())
    );
}

#[tokio::test]
async fn malformed_durable_rows_surface_typed_corruption() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt.db");
    let store = Store::open(&path).await.expect("open store");
    store.bind_session("corrupt").expect("bind store");
    // SQLite WAL permits this second raw connection to inject corrupt rows
    // while the store's long-lived connection remains open.
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");

    raw.execute(
        "INSERT INTO session_meta
         (session_id, session_name, created_at, model, cwd, relation_json)
         VALUES ('corrupt', 'corrupt', 'now', 'model', NULL, '{')",
        [],
    )
    .expect("insert malformed relation");
    assert_corrupt(store.load_session_meta().await, "SessionMeta relation");
    assert_corrupt(store.load_picker_info().await, "SessionMeta relation");
    raw.execute(
        "UPDATE session_meta SET relation_json = ?1 WHERE session_id = 'corrupt'",
        params![serde_json::to_string(&lash_core::SessionRelation::Root).expect("encode relation")],
    )
    .expect("repair relation");

    raw.execute(
        "INSERT INTO graph_nodes
         (seq, session_id, node_id, parent_node_id, generation, frame_node_id, node_json, tombstoned)
         VALUES (1, 'corrupt', 'node', NULL, 0, 'node', '{', 0)",
        [],
    )
    .expect("insert malformed graph node");
    assert_corrupt(store.load_session_graph().await, "SessionGraph node");

    raw.execute(
        "INSERT INTO blobs (hash, content) VALUES ('corrupt-blob', X'C1')",
        [],
    )
    .expect("insert malformed blob");
    let corrupt_blob = BlobRef("corrupt-blob".to_string());
    assert_corrupt(
        store
            .get_typed_blob::<serde_json::Value>(&corrupt_blob)
            .await,
        "typed blob",
    );
    assert_corrupt(
        store.get_checkpoint(&corrupt_blob).await,
        "SessionCheckpoint",
    );
    raw.execute(
        "INSERT INTO blobs (hash, content) VALUES ('corrupt-compressed', ?1)",
        params![
            encode_msgpack(
                &StoredBlobEnvelope {
                    descriptor: BlobArtifactDescriptor::checkpoint_manifest(),
                    compression: BlobCompression::Zlib,
                    content: vec![0xFF, 0x00],
                },
                "corrupt compressed test envelope",
            )
            .expect("encode corrupt compressed test envelope")
        ],
    )
    .expect("insert malformed compressed blob");
    assert_corrupt(
        store
            .get_blob(&BlobRef("corrupt-compressed".to_string()))
            .await,
        "compressed artifact blob",
    );

    raw.pragma_update(None, "ignore_check_constraints", true)
        .expect("allow unknown durable enum injection");
    raw.execute(
        "INSERT INTO attachment_manifest
         (attachment_id, session_id, canonical_uri, intent_at_ms,
          committed_at_ms, owner_kind, owner_id)
         VALUES ('unknown-owner', 'corrupt', 'lash-attachment://unknown', 0,
                 NULL, 'unknown', 'owner')",
        [],
    )
    .expect("insert unknown owner kind");
    assert_corrupt(
        lash_core::AttachmentManifest::list_uncommitted(&store, 0),
        "AttachmentManifest owner kind",
    );

    raw.execute(
        "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
         VALUES (?1, 'dangling-artifact', 'missing-artifact-blob')",
        params![RAW_ARTIFACT_NAMESPACE],
    )
    .expect("insert dangling artifact reference");
    let artifact_error = store
        .get_artifact_bytes("dangling-artifact")
        .await
        .expect_err("dangling artifact reference must fail");
    assert!(
        matches!(
            artifact_error,
            lashlang::ArtifactStoreError::Backend(ref message)
                if message.contains("stored artifact reference data is corrupt")
        ),
        "expected mapped StoredDataCorrupt for dangling artifact reference, got {artifact_error:?}"
    );

    raw.execute(
        "INSERT INTO session_head
         (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
         VALUES ('corrupt', '{', 0, NULL, NULL)",
        [],
    )
    .expect("insert malformed head");
    assert_corrupt(store.load_session_head_meta().await, "SessionHeadMeta");
    assert_corrupt(
        SessionCommitStore::load_session(&store).await,
        "SessionHeadMeta",
    );

    raw.execute(
        "UPDATE session_head
         SET head_json = ?1, checkpoint_ref = 'missing-checkpoint-manifest'
         WHERE session_id = 'corrupt'",
        params![
            encode_json(&SessionHeadPayload {
                session_id: "corrupt".to_string(),
                ..Default::default()
            })
            .expect("encode session head")
        ],
    )
    .expect("install dangling checkpoint reference");
    assert!(matches!(
        SessionCommitStore::load_session(&store).await,
        Err(StoreError::CheckpointComponentMissing {
            key,
            blob_ref,
        }) if key == "manifest" && blob_ref.as_str() == "missing-checkpoint-manifest"
    ));
}

#[tokio::test]
async fn negative_and_exhausted_queued_work_fences_refuse_with_typed_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fence-corrupt.db");
    let store = Store::open(&path).await.expect("open store");
    let session_id = "fence-corrupt";
    let owner = LeaseOwnerIdentity::opaque("owner", "owner:incarnation");
    let lease = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &owner,
            &lash_core::LeaseClaimNonce::new(),
            120_000,
        )
        .await
        .expect("claim session lease")
        .acquired()
        .expect("session lease acquired");
    let batch = store
        .enqueue_queued_work(lash_core::runtime::QueuedWorkBatchDraft::new(
            session_id,
            lash_core::DeliveryPolicy::EarliestSafeBoundary,
            lash_core::SlotPolicy::Exclusive,
            vec![lash_core::runtime::QueuedWorkPayload::session_command(
                lash_core::runtime::SessionCommand::RefreshToolCatalog {
                    reason: "fence test".to_string(),
                },
            )],
        ))
        .await
        .expect("enqueue queued work");
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");

    raw.execute(
        "UPDATE queued_work_batches SET claim_fencing_token = -1 WHERE batch_id = ?1",
        params![batch.batch_id],
    )
    .expect("inject negative fence");
    assert_corrupt(store.list_queued_work(session_id).await, "QueuedWorkBatch");

    raw.execute(
        "UPDATE queued_work_batches SET claim_fencing_token = ?1 WHERE batch_id = ?2",
        params![i64::MAX, batch.batch_id],
    )
    .expect("seed exhausted fence");
    let error = store
        .claim_leading_ready_session_command(session_id, &lease.authority(), &owner)
        .await
        .expect_err("exhausted SQL fence must refuse");
    assert!(matches!(
        error,
        StoreError::MonotonicCounterOverflow {
            counter: "queued_work_claim_fencing_token",
            current,
        } if current == i64::MAX as u64
    ));
}

#[tokio::test]
async fn closed_connection_surfaces_storage_failure_for_every_read_family() {
    let store = Store::memory().await.expect("open store");
    store.bind_session("closed").expect("bind store");
    store.conn.close_for_testing().await;
    let blob_ref = BlobRef("closed".to_string());

    assert_storage_failure("load_session_meta", store.load_session_meta().await);
    assert_storage_failure(
        "load_session_head_meta",
        store.load_session_head_meta().await,
    );
    assert_storage_failure("load_picker_info", store.load_picker_info().await);
    assert_storage_failure("load_session_graph", store.load_session_graph().await);
    assert_storage_failure("get_blob", store.get_blob(&blob_ref).await);
    assert_storage_failure(
        "get_typed_blob",
        store.get_typed_blob::<serde_json::Value>(&blob_ref).await,
    );
    assert_storage_failure("get_checkpoint", store.get_checkpoint(&blob_ref).await);
    assert_storage_failure("load_usage_deltas", store.load_usage_deltas().await);
    assert_storage_failure(
        "SessionCommitStore::load_session",
        SessionCommitStore::load_session(&store).await,
    );
    assert_storage_failure(
        "AttachmentManifest::list_uncommitted",
        lash_core::AttachmentManifest::list_uncommitted(&store, 0),
    );
    assert_storage_failure(
        "AttachmentManifest::list_all_refs",
        lash_core::AttachmentManifest::list_all_refs(&store),
    );
}
