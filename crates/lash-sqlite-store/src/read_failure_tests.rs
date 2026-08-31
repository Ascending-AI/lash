use super::*;
use crate::attachments::RAW_ARTIFACT_NAMESPACE;
use lashlang::LashlangArtifactStore;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

struct WarningCounter(std::sync::Arc<AtomicUsize>);

impl<S> Layer<S> for WarningCounter
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

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

fn assert_artifact_storage_failure<T>(
    label: &str,
    result: Result<T, lashlang::ArtifactStoreError>,
) {
    match result {
        Err(lashlang::ArtifactStoreError::Backend(message)) => assert!(
            message.starts_with("sqlite storage failure:"),
            "expected SQLite storage failure from {label}, got {message}"
        ),
        Err(error) => panic!("expected SQLite storage failure from {label}, got {error}"),
        Ok(_) => panic!("expected SQLite storage failure from {label}"),
    }
}

#[test]
fn turn_failure_settlement_query_filters_receipts_without_evidence() {
    assert!(
        crate::persistence::LOAD_TURN_FAILURE_SETTLEMENTS_SQL
            .contains(r#"result_json LIKE '%"failure_evidence"%'"#),
        "the SQL path must exclude receipts that cannot carry failure evidence"
    );
}

#[tokio::test]
async fn turn_failure_reopen_skips_one_corrupt_evidence_receipt_among_many_receipts() {
    const SESSION_ID: &str = "failure-evidence-corrupt-receipt";
    const NON_EVIDENCE_RECEIPTS: usize = 256;

    let store = Store::memory().await.expect("open receipt-filter store");
    store
        .bind_session(SESSION_ID)
        .expect("bind receipt-filter store");
    let state = lash_core::RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.failure_evidence = vec![lash_core::TurnFailureEvidence {
        partial_output: Some(lash_core::TurnFailurePartialOutput::Complete {
            text: "settled partial output".to_string(),
        }),
        billed_usage: lash_core::llm::types::LlmUsage {
            output_tokens: 3,
            ..Default::default()
        },
        refusal: lash_core::ChargeSafetyRefusalEvidence {
            code: "unsafe_retry_after_output_started".to_string(),
            denial_reason: lash_core::ChargeSafetyDenialReason::GuaranteeRequired,
            protocol_position: lash_core::ProtocolPosition::OutputStarted,
            attempt_number: 1,
            attempt_count: 1,
        },
    }];
    store
        .commit_runtime_state(commit)
        .await
        .expect("seed one failure-evidence receipt");

    let evidence_turn_id = store
        .conn
        .call(|conn| {
            let (turn_id, result_json, committed_at_ms) = conn.query_row(
                "SELECT turn_id, result_json, committed_at_ms
                 FROM runtime_turn_commits
                 WHERE session_id = ?1",
                params![SESSION_ID],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )?;
            let mut no_evidence: serde_json::Value =
                serde_json::from_str(&result_json).expect("seed receipt is valid JSON");
            no_evidence
                .as_object_mut()
                .expect("receipt JSON is an object")
                .remove("failure_evidence");
            let no_evidence = serde_json::to_string(&no_evidence)
                .expect("serialize receipt without failure evidence");
            for index in 0..NON_EVIDENCE_RECEIPTS {
                conn.execute(
                    "INSERT INTO runtime_turn_commits
                     (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        SESSION_ID,
                        format!("non-evidence-{index:03}"),
                        format!("non-evidence-hash-{index:03}"),
                        no_evidence,
                        committed_at_ms + i64::try_from(index).expect("receipt index") + 1,
                    ],
                )?;
            }
            conn.execute(
                "INSERT INTO runtime_turn_commits
                 (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms)
                 VALUES (?1, 'corrupt-evidence', 'corrupt-evidence-hash', ?2, ?3)",
                params![
                    SESSION_ID,
                    r#"{"failure_evidence":"#,
                    committed_at_ms
                        + i64::try_from(NON_EVIDENCE_RECEIPTS).expect("receipt count")
                        + 1,
                ],
            )?;
            Ok(turn_id)
        })
        .await
        .expect("seed many ordinary receipts and one corrupt evidence receipt");

    let warning_count = std::sync::Arc::new(AtomicUsize::new(0));
    let subscriber =
        Registry::default().with(WarningCounter(std::sync::Arc::clone(&warning_count)));
    let reopened = store
        .load_session()
        .with_subscriber(subscriber)
        .await
        .expect("one corrupt evidence receipt must not make the session unreadable")
        .expect("seeded session remains readable");

    assert_eq!(
        reopened.turn_failure_settlements.len(),
        1,
        "only the valid evidence receipt becomes a settlement"
    );
    assert_eq!(
        reopened.turn_failure_settlements[0].turn_id,
        evidence_turn_id
    );
    assert_eq!(
        warning_count.load(Ordering::Relaxed),
        1,
        "the skipped corrupt evidence receipt emits one warning"
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
}

#[tokio::test]
async fn corrupt_non_msgpack_blob_surfaces_stored_data_corrupt_from_get_blob() {
    let store = Store::memory().await.expect("open store");
    let blob_ref = BlobRef("corrupt-non-msgpack-blob".to_string());
    let blob_hash = blob_ref.as_str().to_string();
    let raw_content = b"bare blob body is not an artifact envelope".to_vec();
    store
        .conn
        .call(move |conn| {
            conn.execute(
                "INSERT INTO blobs (hash, content) VALUES (?1, ?2)",
                params![blob_hash, raw_content],
            )?;
            Ok(())
        })
        .await
        .expect("seed corrupt blob");

    assert_corrupt(store.get_blob(&blob_ref).await, "artifact blob envelope");
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

    // `ck_session_meta_relation_kind` makes this row unreachable through any
    // ordinary write, which is exactly what the constraint is for. The read-side
    // detector still has to hold: a catalog restored from a pre-CHECK dump, or
    // one a host ALTERed, can present the byte the driver must refuse to decode.
    raw.pragma_update(None, "ignore_check_constraints", true)
        .expect("permit manufacturing a row the DDL now forbids");
    raw.execute(
        "INSERT INTO session_meta
         (session_id, relation_kind)
         VALUES ('corrupt', 'corrupt')",
        [],
    )
    .expect("insert malformed relation");
    raw.pragma_update(None, "ignore_check_constraints", false)
        .expect("restore CHECK enforcement");
    assert_corrupt(store.load_session_meta().await, "SessionMeta relation");
    raw.execute(
        "UPDATE session_meta SET relation_kind = 'root' WHERE session_id = 'corrupt'",
        [],
    )
    .expect("repair relation");

    raw.execute(
        "INSERT INTO graph_nodes
         (session_id, node_id, parent_node_id, generation, frame_node_id, node_json, tombstoned)
         VALUES ('corrupt', 'node', NULL, 0, 'node', '{', 0)",
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
        "artifact blob envelope",
    );
    assert_corrupt(
        store.get_checkpoint(&corrupt_blob).await,
        "artifact blob envelope",
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
            "read-failure-executor",
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

async fn readonly_store_for_blob_write_failure() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("readonly.db");
    Store::open(&path).await.expect("provision store");
    let store = Store::open_readonly(&path)
        .await
        .expect("open read-only store");
    (dir, store)
}

#[tokio::test]
async fn readonly_connection_rejects_every_surviving_blob_write_path() {
    let (_dir, store) = readonly_store_for_blob_write_failure().await;
    assert_storage_failure(
        "put_unrooted_artifact_blob_for_testing",
        store
            .put_unrooted_artifact_blob_for_testing(
                BlobArtifactDescriptor::checkpoint_component(),
                b"artifact",
            )
            .await,
    );

    assert_artifact_storage_failure(
        "put_artifact_ref_blob",
        store
            .put_artifact_bytes("readonly-artifact", "generic", b"artifact-ref")
            .await,
    );

    let trigger_artifact = lashlang::ModuleArtifact::from_program(
        lashlang::parse("process readonly(root: str) { finish root }")
            .expect("parse trigger artifact"),
    )
    .expect("build trigger artifact");
    assert_artifact_storage_failure(
        "replace_current_trigger_manifest",
        store
            .replace_current_trigger_manifest("readonly-owner", &trigger_artifact)
            .await,
    );

    let state = lash_core::RuntimeSessionState {
        session_id: "readonly-session".to_string(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    assert_storage_failure(
        "commit_runtime_state",
        store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await,
    );

    let raw = store
        .conn
        .call(|conn| {
            Store::insert_artifact_blob_conn(
                conn,
                BlobArtifactDescriptor::checkpoint_component(),
                b"raw",
                BuiltinBlobProfile::LowLatency,
            )
        })
        .await
        .map_err(sqlite_error);
    assert_storage_failure("insert_artifact_blob_conn", raw);

    let typed = store
        .conn
        .call(|conn| {
            Store::put_typed_artifact_blob_conn(
                conn,
                BlobArtifactDescriptor::checkpoint_component(),
                &42_u64,
                BuiltinBlobProfile::LowLatency,
            )
            .map_err(sqlite_conversion_error)
        })
        .await
        .map_err(sqlite_error);
    assert_storage_failure("put_typed_artifact_blob_conn", typed);

    assert_storage_failure(
        "put_checkpoint",
        store
            .put_checkpoint(&HydratedSessionCheckpoint::default())
            .await,
    );
}
