use super::*;
use crate::artifact_store::MODULE_ARTIFACT_NAMESPACE;
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

#[tokio::test]
async fn sqlite_persisted_record_decode_classification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("persisted-record-decode.db");

    let head_session_id = SessionId::from("persisted-record-decode-head");
    let head_store = Store::open(&path).await.expect("open head store");
    head_store
        .bind_session(&head_session_id)
        .expect("bind head store");
    head_store
        .admit_and_bind_session(&lash_core_execution::SessionBinding::root(
            head_session_id.as_str(),
        ))
        .await
        .expect("admit head session");
    let head_state = lash_core_execution::RuntimeSessionState {
        session_id: head_session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    head_store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&head_state, &[]),
        )
        .await
        .expect("seed head session");

    let checkpoint_session_id = SessionId::from("persisted-record-decode-checkpoint");
    let checkpoint_store = Store::open(&path).await.expect("open checkpoint store");
    checkpoint_store
        .bind_session(&checkpoint_session_id)
        .expect("bind checkpoint store");
    checkpoint_store
        .admit_and_bind_session(&lash_core_execution::SessionBinding::root(
            checkpoint_session_id.as_str(),
        ))
        .await
        .expect("admit checkpoint session");
    let checkpoint_state = lash_core_execution::RuntimeSessionState {
        session_id: checkpoint_session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    checkpoint_store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&checkpoint_state, &[]),
        )
        .await
        .expect("seed checkpoint session");

    let raw = rusqlite::Connection::open(&path).expect("open corruption connection");
    assert_eq!(
        raw.execute(
            "UPDATE session_head SET head_json = '{' WHERE session_id = ?1",
            [head_session_id.as_str()],
        )
        .expect("corrupt head JSON"),
        1
    );
    assert_corrupt(head_store.load_session_head_meta().await, "SessionHeadMeta");

    let checkpoint_ref: String = raw
        .query_row(
            "SELECT checkpoint_ref FROM session_head WHERE session_id = ?1",
            [checkpoint_session_id.as_str()],
            |row| row.get(0),
        )
        .expect("read checkpoint ref");
    let malformed_manifest = encode_msgpack(
        &StoredBlobEnvelope {
            compression: BlobCompression::None,
            content: vec![0xc1],
        },
        "malformed checkpoint fixture",
    )
    .expect("encode valid blob envelope");
    assert_eq!(
        raw.execute(
            "UPDATE blobs SET content = ?1 WHERE hash = ?2",
            params![malformed_manifest, checkpoint_ref],
        )
        .expect("corrupt checkpoint MessagePack"),
        1
    );
    assert_corrupt(
        SessionCommitStore::load_session(&checkpoint_store).await,
        "SessionCheckpoint",
    );
}

#[test]
fn turn_failure_settlement_query_filters_receipts_without_evidence() {
    assert!(
        crate::session_sql::session_sql()
            .turn_commits
            .select_failure_settlements
            .sql()
            .contains(r#"result_json LIKE '%"failure_evidence"%'"#),
        "the SQL path must exclude receipts that cannot carry failure evidence"
    );
}

/// Seed one committed session carrying failure evidence, then splice an extra
/// receipt row into `runtime_turn_commits` under the `bad-evidence-receipt`
/// operation key so a refusal can be asserted against that exact row.
async fn seed_failure_evidence_session(session_id: &str, bad_result_json: &str) -> Store {
    let store = Store::memory().await.expect("open receipt store");
    store
        .bind_session(&SessionId::from(session_id))
        .expect("bind receipt store");
    let state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.failure_evidence = vec![lash_core_execution::TurnFailureEvidence {
        partial_output: Some(lash_core_execution::TurnFailurePartialOutput::Complete {
            text: "settled partial output".to_string(),
        }),
        billed_usage: lash_core_execution::llm::types::LlmUsage {
            output_tokens: 3,
            ..Default::default()
        },
        refusal: lash_core_execution::ChargeSafetyRefusalEvidence {
            code: "unsafe_retry_after_output_started".to_string(),
            denial_reason: lash_core_execution::ChargeSafetyDenialReason::GuaranteeRequired,
            protocol_position: lash_core_execution::ProtocolPosition::OutputStarted,
            attempt_number: 1,
            attempt_count: 1,
        },
    }];
    store
        .commit_runtime_state(commit)
        .await
        .expect("seed one failure-evidence receipt");

    let session_id = session_id.to_string();
    let bad_result_json = bad_result_json.to_string();
    store
        .conn
        .call(move |conn| {
            let committed_at_ms = conn.query_row(
                "SELECT committed_at_ms FROM runtime_turn_commits WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, i64>(0),
            )?;
            conn.execute(
                "INSERT INTO runtime_turn_commits
                 (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms)
                 VALUES (?1, 'bad-evidence-receipt', 'bad-evidence-hash', ?2, ?3)",
                params![session_id, bad_result_json, committed_at_ms + 1],
            )?;
            Ok(())
        })
        .await
        .expect("splice the bad receipt row");
    store
}

#[tokio::test]
async fn turn_failure_reopen_refuses_one_corrupt_evidence_receipt() {
    const SESSION_ID: &str = "failure-evidence-corrupt-receipt";
    let store = seed_failure_evidence_session(SESSION_ID, r#"{"failure_evidence":"#).await;

    let error = store
        .load_session()
        .await
        .expect_err("a corrupt evidence receipt must refuse the whole load");
    assert!(
        matches!(
            &error,
            StoreError::StoredDataCorrupt {
                record_kind: "RuntimeCommitReceipt",
                message,
            } if message.contains(SESSION_ID) && message.contains("bad-evidence-receipt")
        ),
        "the corrupt receipt refusal must name the session and operation: {error:?}"
    );
}

#[tokio::test]
async fn turn_failure_reopen_refuses_a_preversioned_receipt() {
    const SESSION_ID: &str = "failure-evidence-preversioned-receipt";
    let store = seed_failure_evidence_session(SESSION_ID, r#"{"failure_evidence":[{}]}"#).await;

    let error = store
        .load_session()
        .await
        .expect_err("an unversioned receipt must refuse the whole load");
    assert!(
        matches!(
            &error,
            StoreError::MissingRecordSchemaVersion {
                record_kind: "RuntimeCommitReceipt",
                ..
            }
        ),
        "the pre-versioned receipt refusal must be typed: {error:?}"
    );
}

#[tokio::test]
async fn turn_failure_reopen_refuses_a_newer_receipt_version() {
    const SESSION_ID: &str = "failure-evidence-newer-receipt";
    let newer = lash_core_execution::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION + 1;
    let bad_json = format!(r#"{{"schema_version":{newer},"failure_evidence":[{{}}]}}"#);
    let store = seed_failure_evidence_session(SESSION_ID, &bad_json).await;

    let error = store
        .load_session()
        .await
        .expect_err("a newer receipt version must refuse the whole load");
    assert!(
        matches!(
            &error,
            StoreError::UnsupportedRecordSchemaVersion {
                record_kind: "RuntimeCommitReceipt",
                actual,
                expected,
            } if *actual == newer
                && *expected == lash_core_execution::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION
        ),
        "the newer-version receipt refusal must be the typed version error: {error:?}"
    );
}

#[tokio::test]
async fn absent_rows_remain_honest_successful_outcomes() {
    let store = Store::memory().await.expect("open store");
    store
        .bind_session(&SessionId::from("absent"))
        .expect("bind store");
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
        lash_core_execution::AttachmentManifest::list_uncommitted(&store, 0)
            .await
            .expect("list uncommitted attachments")
            .is_empty()
    );
    assert!(
        lash_core_execution::AttachmentManifest::list_all_refs(&store)
            .await
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
async fn unknown_attachment_owner_kind_refuses_with_canonical_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unknown-attachment-owner.db");
    let store = Store::open(&path).await.expect("open store");
    store
        .bind_session(&SessionId::from("unknown-attachment-owner"))
        .expect("bind store");
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");
    raw.pragma_update(None, "ignore_check_constraints", true)
        .expect("allow unknown durable enum injection");
    raw.execute(
        "INSERT INTO attachment_manifest
         (attachment_id, session_id, canonical_uri, intent_at_ms,
          committed_at_ms, owner_kind, owner_id)
         VALUES ('unknown-owner', 'unknown-attachment-owner',
                 'lash-attachment://unknown', 0, NULL, 'unknown', 'owner')",
        [],
    )
    .expect("insert unknown owner kind");

    let error = lash_core_execution::AttachmentManifest::list_uncommitted(&store, 0)
        .await
        .expect_err("unknown SQLite attachment owner kind must refuse");
    assert!(
        matches!(
            error,
            StoreError::StoredDataCorrupt {
                record_kind: "AttachmentManifest owner kind",
                ref message,
            } if message == "unknown attachment owner kind `unknown`"
        ),
        "SQLite must return the canonical attachment-owner corruption refusal, got {error:?}"
    );
}

#[tokio::test]
async fn bare_process_attachment_owner_refuses_with_canonical_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bare-process-attachment-owner.db");
    let store = Store::open(&path).await.expect("open store");
    store
        .bind_session(&SessionId::from("bare-process-attachment-owner"))
        .expect("bind store");
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");
    raw.pragma_update(None, "ignore_check_constraints", true)
        .expect("allow predecessor owner injection");
    raw.execute(
        "INSERT INTO attachment_manifest
         (attachment_id, session_id, canonical_uri, intent_at_ms,
          committed_at_ms, owner_kind, owner_id, owner_incarnation)
         VALUES ('bare-process-owner', 'bare-process-attachment-owner',
                 'lash-attachment://bare-process', 0, NULL, 'process', 'process-1', NULL)",
        [],
    )
    .expect("insert bare process owner");

    let error = lash_core_execution::AttachmentManifest::list_uncommitted(&store, 0)
        .await
        .expect_err("bare SQLite process attachment owner must refuse");
    assert!(
        matches!(
            error,
            StoreError::StoredDataCorrupt {
                record_kind: "AttachmentManifest owner",
                ref message,
            } if message == "process attachment owner `process-1` has no incarnation; bare process-owner identities are unsupported"
        ),
        "SQLite must return the canonical bare-process-owner refusal, got {error:?}"
    );
}

#[tokio::test]
async fn malformed_durable_rows_surface_typed_corruption() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt.db");
    let store = Store::open(&path).await.expect("open store");
    store
        .bind_session(&SessionId::from("corrupt"))
        .expect("bind store");
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
         (session_id, relation_kind, session_state_version)
         VALUES ('corrupt', 'corrupt', ?1)",
        [i64::from(
            lash_core_execution::store::CURRENT_SESSION_STATE_VERSION,
        )],
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
        lash_core_execution::AttachmentManifest::list_uncommitted(&store, 0).await,
        "AttachmentManifest owner kind",
    );

    raw.execute(
        "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
         VALUES (?1, 'dangling-artifact', 'missing-artifact-blob')",
        params![MODULE_ARTIFACT_NAMESPACE],
    )
    .expect("insert dangling artifact reference");
    let dangling_ref: lashlang::ModuleRef =
        serde_json::from_value(serde_json::json!("dangling-artifact")).unwrap();
    let artifact_error = store
        .get_module_artifact(&dangling_ref)
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
                session_id: SessionId::from("corrupt"),
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
            &SessionId::from(session_id),
            &owner,
            "read-failure-executor",
            &lash_core_execution::LeaseClaimNonce::new(),
            120_000,
        )
        .await
        .expect("claim session lease")
        .acquired()
        .expect("session lease acquired");
    let batch = store
        .enqueue_queued_work(lash_core_execution::runtime::QueuedWorkBatchDraft::new(
            session_id,
            lash_core_execution::DeliveryPolicy::EarliestSafeBoundary,
            lash_core_execution::runtime::SessionCommand::RefreshToolCatalog {
                reason: "fence test".to_string(),
            },
        ))
        .await
        .expect("enqueue queued work");
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");

    raw.execute(
        "UPDATE queued_work_batches SET claim_fencing_token = -1 WHERE batch_id = ?1",
        params![batch.batch_id.as_str()],
    )
    .expect("inject negative fence");
    assert_corrupt(
        store.list_queued_work(&SessionId::from(session_id)).await,
        "QueuedWorkBatch",
    );

    raw.execute(
        "UPDATE queued_work_batches SET claim_fencing_token = ?1 WHERE batch_id = ?2",
        params![i64::MAX, batch.batch_id.as_str()],
    )
    .expect("seed exhausted fence");
    let error = store
        .claim_leading_ready_session_command(
            &SessionId::from(session_id),
            &lease.authority(),
            &owner,
        )
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
    store
        .bind_session(&SessionId::from("closed"))
        .expect("bind store");
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
        lash_core_execution::AttachmentManifest::list_uncommitted(&store, 0).await,
    );
    assert_storage_failure(
        "AttachmentManifest::list_all_refs",
        lash_core_execution::AttachmentManifest::list_all_refs(&store).await,
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
        "publish_module_artifact",
        store
            .publish_module_artifact(
                &lash_core_execution::ArtifactOwner::host("readonly-test"),
                &lashlang::ModuleArtifact::from_program({
                    use lashlang::testing::ast_builders as b;

                    b::program(vec![b::finish(b::bool_lit(true))])
                })
                .expect("build module"),
            )
            .await,
    );

    let state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from("readonly-session"),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
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

#[tokio::test]
async fn queued_work_hydration_rejects_kind_payload_contradiction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("family-corrupt.db");
    let store = Store::open(&path).await.expect("open store");
    let batch = store
        .enqueue_queued_work(lash_core_execution::runtime::QueuedWorkBatchDraft::new(
            "family-corrupt",
            lash_core_execution::DeliveryPolicy::EarliestSafeBoundary,
            lash_core_execution::runtime::SessionCommand::RefreshToolCatalog {
                reason: "family test".into(),
            },
        ))
        .await
        .expect("enqueue command");
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");
    raw.execute(
        "UPDATE queued_work_batches SET work_kind = 'turn' WHERE batch_id = ?1",
        params![batch.batch_id.as_str()],
    )
    .expect("contradict stored family");
    assert_corrupt(
        store
            .list_queued_work(&SessionId::from("family-corrupt"))
            .await,
        "QueuedWorkBatch",
    );
}

/// Hydrating a queued-work batch reads two tables: the batch row, then its
/// item rows. Both reads must come from one snapshot.
///
/// FIG-3017: they used to run in autocommit, so each took its own snapshot and
/// another connection's commit could land between them. A batch consumed in
/// that window was returned as a header with no payloads, and the reader
/// reported `StoredDataCorrupt { record_kind: "QueuedWorkBatch", message:
/// "queued work requires at least one payload" }` — a live write reported as
/// corruption. The window is one commit wide, so the seam, not load, is what
/// drives it: `pause_queued_work_hydration` stops the read between the two
/// statements and the delete commits from a second connection while it waits.
#[derive(Clone, Copy)]
enum QueuedWorkRead {
    All,
    Pending,
}

async fn queued_work_read_survives_a_consume_mid_hydration(session_id: &str, read: QueuedWorkRead) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("queued-work-snapshot.db");
    let injector = crate::testing::SqliteFaultInjector::default();
    let store = Arc::new(
        Store::open_with_options_clock_and_process_registry(
            &path,
            StoreOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            None,
            None,
            Some(injector.clone()),
        )
        .await
        .expect("open store with a read seam"),
    );
    let session_id = SessionId::from(session_id);
    let batch = store
        .enqueue_queued_work(lash_core_execution::runtime::QueuedWorkBatchDraft::new(
            session_id.as_str(),
            lash_core_execution::DeliveryPolicy::EarliestSafeBoundary,
            lash_core_execution::runtime::SessionCommand::RefreshToolCatalog {
                reason: "snapshot test".into(),
            },
        ))
        .await
        .expect("enqueue queued work");

    let pause = injector.pause_queued_work_hydration();
    let reader = tokio::spawn({
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        async move {
            match read {
                QueuedWorkRead::All => store.list_queued_work(&session_id).await,
                QueuedWorkRead::Pending => store.list_pending_queued_work(&session_id).await,
            }
        }
    });
    pause.wait_until_reached().await;

    // A second connection consumes the batch while the read is between its two
    // statements. The cascade takes the item rows with the batch row, which is
    // what the reader must not observe as a batch without payloads.
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");
    raw.busy_timeout(std::time::Duration::from_millis(15_000))
        .expect("raw busy timeout");
    raw.execute_batch("PRAGMA foreign_keys=ON;")
        .expect("raw foreign keys");
    let deleted = raw
        .execute(
            "DELETE FROM queued_work_batches WHERE batch_id = ?1",
            params![batch.batch_id.as_str()],
        )
        .expect("consume the batch from a second connection");
    assert_eq!(deleted, 1, "the competing commit must land in the window");
    pause.release();

    let batches = reader
        .await
        .expect("reader task")
        .expect("a consumed batch is not corrupt data");
    assert_eq!(batches.len(), 1, "the read holds its own snapshot");
    assert_eq!(batches[0].batch_id.as_str(), batch.batch_id.as_str());
    assert!(
        !batches[0].items.is_empty(),
        "a batch row and its item rows come from one snapshot"
    );
    // The consume really did commit: the next read no longer sees it.
    assert!(
        store
            .list_queued_work(&session_id)
            .await
            .expect("read after the consume")
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_queued_work_survives_a_consume_mid_hydration() {
    queued_work_read_survives_a_consume_mid_hydration("queued-snapshot-all", QueuedWorkRead::All)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_pending_queued_work_survives_a_consume_mid_hydration() {
    queued_work_read_survives_a_consume_mid_hydration(
        "queued-snapshot-pending",
        QueuedWorkRead::Pending,
    )
    .await;
}
