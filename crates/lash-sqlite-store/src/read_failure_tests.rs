use super::*;
use crate::artifact_store::MODULE_ARTIFACT_NAMESPACE;
use lash_core_execution::FleetFormat;
use lash_core_execution::store::{StoreTestSupport as _, WindowSelector};
use lash_core_execution::{
    ModuleArtifactStore, QueuedWorkStore as _, SessionCatalogStore as _, SessionHistoryStore as _,
};

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
    result: Result<T, lash_core::ArtifactStoreError>,
) {
    match result {
        Err(lash_core::ArtifactStoreError::Backend(message)) => assert!(
            message.starts_with("sqlite storage failure:"),
            "expected SQLite storage failure from {label}, got {message}"
        ),
        Err(error) => panic!("expected SQLite storage failure from {label}, got {error}"),
        Ok(_) => panic!("expected SQLite storage failure from {label}"),
    }
}

#[tokio::test]
async fn corrupt_non_msgpack_blob_surfaces_stored_data_corrupt_from_get_blob() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open store");
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
async fn blob_envelope_refuses_an_unknown_version_and_keeps_the_bytes() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open blob store");
    for (name, version, compression) in [
        (
            "future-version-blob",
            SQLITE_BLOB_ENVELOPE_VERSION + 1,
            "None",
        ),
        (
            "future-compression-blob",
            SQLITE_BLOB_ENVELOPE_VERSION,
            "future-codec",
        ),
    ] {
        let encoded = encode_msgpack(
            &StoredBlobEnvelope {
                version,
                compression: compression.to_string(),
                content: b"kept bytes".to_vec(),
            },
            "future blob fixture",
        )
        .expect("encode future envelope");
        let hash = name.to_string();
        let bytes = encoded.clone();
        store
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO blobs (hash, content) VALUES (?1, ?2)",
                    params![hash, bytes],
                )?;
                Ok(())
            })
            .await
            .expect("seed future blob");

        let refusal = store
            .get_blob(&BlobRef(name.to_string()))
            .await
            .expect_err("future blob must refuse");
        if version != SQLITE_BLOB_ENVELOPE_VERSION {
            assert!(matches!(
                refusal,
                StoreError::UnsupportedRecordSchemaVersion {
                    record_kind: "SQLite stored blob envelope",
                    actual,
                    expected: SQLITE_BLOB_ENVELOPE_VERSION,
                } if actual == version
            ));
        } else {
            assert!(matches!(refusal, StoreError::Incompatible { .. }));
        }
        let hash = name.to_string();
        let persisted: Vec<u8> = store
            .conn
            .call(move |conn| {
                conn.query_row("SELECT content FROM blobs WHERE hash = ?1", [hash], |row| {
                    row.get(0)
                })
            })
            .await
            .expect("read retained bytes");
        assert_eq!(
            persisted, encoded,
            "refusal must leave the stored blob intact"
        );
    }
}

async fn readonly_store_for_blob_write_failure() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("readonly.db");
    let writable = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("provision store");
    writable
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&SessionId::from(
                "readonly-session",
            )),
        )
        .await
        .expect("admit session before the read-only test");
    let store =
        SqliteStore::open_readonly(&crate::location::DatabaseLocation::standalone_file(&path))
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

    assert_artifact_storage_failure("publish_module_artifact", {
        let module = lashlang::ModuleArtifact::from_program({
            use lashlang::testing::ast_builders as b;

            b::program(vec![b::finish(b::bool_lit(true))])
        })
        .expect("build module");
        store
            .publish_module_artifact(
                &lash_core_execution::ReferrerClaim::unguarded(
                    lash_core_execution::ArtifactReferrer::HostPin(
                        lash_core_execution::HostArtifactPin::mint(),
                    ),
                )
                .expect("host pin claim"),
                module.module_ref().as_str(),
                &module.to_store_bytes().expect("encode module"),
            )
            .await
    });

    let state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from("readonly-session"),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    assert_storage_failure(
        "commit_runtime_state",
        store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
            .await,
    );

    let raw = store
        .conn
        .call(|conn| {
            SqliteStore::insert_artifact_blob_conn(
                conn,
                BlobArtifactDescriptor::checkpoint_component(),
                b"raw",
                BuiltinBlobProfile::LowLatency,
                FleetFormat::current(),
            )
        })
        .await
        .map_err(sqlite_error);
    assert_storage_failure("insert_artifact_blob_conn", raw);

    let typed = store
        .conn
        .call(|conn| {
            SqliteStore::put_typed_artifact_blob_conn(
                conn,
                BlobArtifactDescriptor::checkpoint_component(),
                &42_u64,
                BuiltinBlobProfile::LowLatency,
                FleetFormat::current(),
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
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open store");
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
    // Plant corruption past the SQL CHECK to exercise the row decoder.
    raw.pragma_update(None, "ignore_check_constraints", true)
        .expect("allow the planted family contradiction");
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

/// A queued batch and its payload come from one row snapshot. A competing
/// delete after that read must preserve the fetched payload until the reader
/// returns; the next read sees the deletion.
#[derive(Clone, Copy)]
enum QueuedWorkRead {
    All,
    Pending,
}

async fn queued_work_read_survives_a_consume_mid_hydration(session_id: &str, read: QueuedWorkRead) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("queued-work-snapshot.db");
    let pauses = crate::testing::SqlitePauses::default();
    let store = Arc::new(
        SqliteStore::open_at(
            &crate::location::DatabaseLocation::standalone_file(&path),
            StoreOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            None,
            lash_core_execution::FleetFormat::writable(),
            Some(pauses.clone()),
        )
        .await
        .expect("open store with a read seam"),
    );
    let session_id = SessionId::fixture(session_id);
    let batch = store
        .enqueue_queued_work(lash_core_execution::runtime::QueuedWorkBatchDraft::new(
            session_id.clone(),
            lash_core_execution::DeliveryPolicy::EarliestSafeBoundary,
            lash_core_execution::runtime::SessionCommand::RefreshToolCatalog {
                reason: "snapshot test".into(),
            },
        ))
        .await
        .expect("enqueue queued work");

    let pause = pauses.pause_queued_work_hydration();
    let reader = tokio::spawn({
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        async move {
            match read {
                QueuedWorkRead::All => store.list_queued_work(&session_id).await,
                QueuedWorkRead::Pending => store.list_open_queued_work(&session_id).await,
            }
        }
    });
    pause.wait_until_reached().await;

    // A second connection consumes the batch after the reader fetched its row.
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
    assert_eq!(
        serde_json::to_value(&batches[0].payload).expect("record the read payload"),
        serde_json::to_value(&batch.payload).expect("record the submitted payload"),
        "the batch and its payload come from one snapshot"
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

async fn admit_and_seed(store: &SqliteStore, session_id: &SessionId) {
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(session_id),
        )
        .await
        .expect("admit the seeded session");
    let mut state = lash_core_execution::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    store
        .commit_runtime_state(lash_core_execution::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("seed the session head");
}

#[tokio::test]
async fn sqlite_persisted_record_decode_classification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("persisted-record-decode.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open store");
    let head_session_id = SessionId::from("persisted-record-decode-head");
    let checkpoint_session_id = SessionId::from("persisted-record-decode-checkpoint");
    admit_and_seed(&store, &head_session_id).await;
    admit_and_seed(&store, &checkpoint_session_id).await;

    let raw = rusqlite::Connection::open(&path).expect("open corruption connection");
    assert_eq!(
        raw.execute(
            "UPDATE session_revisions SET head_json = '{' WHERE session_id = ?1 AND head_revision = (SELECT head_revision FROM session_head WHERE session_id = ?1)",
            [head_session_id.as_str()],
        )
        .expect("corrupt head JSON"),
        1
    );
    assert_corrupt(
        store.load_session_head_meta(&head_session_id).await,
        "SessionHeadMeta",
    );

    let checkpoint_ref: String = raw
        .query_row(
            "SELECT checkpoint_ref FROM session_head JOIN session_revisions USING (session_id, head_revision) WHERE session_id = ?1",
            [checkpoint_session_id.as_str()],
            |row| row.get(0),
        )
        .expect("read checkpoint ref");
    let malformed_manifest = encode_msgpack(
        &StoredBlobEnvelope {
            version: SQLITE_BLOB_ENVELOPE_VERSION,
            compression: "None".to_string(),
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
        store
            .load_session_window(&checkpoint_session_id, WindowSelector::Current)
            .await,
        "SessionCheckpoint",
    );
}

/// Seed one committed session carrying failure evidence, then splice an extra
/// receipt row into `runtime_turn_commits` under the `bad-evidence-receipt`
/// operation key so a refusal can be asserted against that exact row.
async fn seed_failure_evidence_session(
    session_id: &str,
    bad_result_json: &str,
) -> Arc<SqliteStore> {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open receipt store");
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(
                &SessionId::fixture(session_id),
            ),
        )
        .await
        .expect("admit receipt session");
    let state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    let mut commit = lash_core_execution::RuntimeCommit::persisted_state_for_test(&state);
    commit.failure_evidence = vec![lash_core_execution::TurnFailureEvidence {
        partial_output: Some(lash_core_execution::TurnFailurePartialOutput::Complete {
            text: "settled partial output".to_string(),
        }),
        billed_usage: lash_core_execution::llm::types::LlmUsage {
            output_tokens: 3,
            ..Default::default()
        },
        refusal: lash_core_execution::ChargeSafetyRefusalEvidence {
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
                 (session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                  failure_evidence, change_seq)
                 VALUES (?1, 'bad-evidence-receipt', 'bad-evidence-hash', ?2, ?3, 1, (SELECT current_seq + 1 FROM turn_change_clock))",
                params![session_id, bad_result_json, committed_at_ms + 1],
            )?;
            conn.execute("UPDATE turn_change_clock SET current_seq = current_seq + 1 WHERE singleton = 1", [])?;
            Ok(())
        })
        .await
        .expect("splice the bad receipt row");
    store
}

async fn failure_evidence_refusal(store: &SqliteStore, session_id: &str) -> StoreError {
    store
        .load_failure_evidence_page(
            &SessionId::fixture(session_id),
            None,
            std::num::NonZeroU32::new(100).expect("nonzero page limit"),
        )
        .await
        .expect_err("a bad evidence receipt must refuse the page")
}

#[tokio::test]
async fn turn_failure_evidence_refuses_one_corrupt_receipt() {
    const SESSION_ID: &str = "failure-evidence-corrupt-receipt";
    let store = seed_failure_evidence_session(SESSION_ID, r#"{"failure_evidence":"#).await;

    let error = failure_evidence_refusal(&store, SESSION_ID).await;
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
async fn turn_failure_evidence_refuses_a_preversioned_receipt() {
    const SESSION_ID: &str = "failure-evidence-preversioned-receipt";
    let store = seed_failure_evidence_session(SESSION_ID, r#"{"failure_evidence":[{}]}"#).await;

    let error = failure_evidence_refusal(&store, SESSION_ID).await;
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
async fn turn_failure_evidence_refuses_a_newer_receipt_version() {
    const SESSION_ID: &str = "failure-evidence-newer-receipt";
    let newer = lash_core_execution::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION + 1;
    let bad_json = format!(r#"{{"schema_version":{newer},"failure_evidence":[{{}}]}}"#);
    let store = seed_failure_evidence_session(SESSION_ID, &bad_json).await;

    let error = failure_evidence_refusal(&store, SESSION_ID).await;
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
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open store");
    let unknown = SessionId::from("never-admitted");
    assert_eq!(
        store
            .lookup_session(&unknown)
            .await
            .expect("look up an unknown session"),
        lash_core_execution::SessionLookup::Absent
    );
    assert!(
        store
            .load_session_meta(&unknown)
            .await
            .expect("read metadata")
            .is_none()
    );

    let admitted = SessionId::from("admitted-without-a-head");
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&admitted),
        )
        .await
        .expect("admit a session");
    store
        .delete_session_head_for_testing(&admitted)
        .await
        .expect("leave the catalog row without its head");
    assert!(
        store
            .load_session_head_meta(&admitted)
            .await
            .expect("read head")
            .is_none()
    );
    assert!(
        store
            .load_session_window(&admitted, WindowSelector::Current)
            .await
            .expect("read window")
            .is_none()
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
        lash_core_execution::AttachmentReferrers::attachment_referrers(
            store.as_ref(),
            &lash_core_execution::AttachmentId::parse("absent").unwrap()
        )
        .await
        .expect("list uncommitted attachments")
        .is_empty()
    );
    assert!(
        lash_core_execution::AttachmentRootSet::live_attachment_refs(store.as_ref())
            .await
            .expect("list attachment refs")
            .is_empty()
    );
}

#[tokio::test]
async fn malformed_durable_rows_surface_typed_corruption() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open store");
    let session_id = SessionId::from("corrupt");
    // SQLite WAL permits this second raw connection to inject corrupt rows
    // while the store's long-lived connections remain open.
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
    assert_corrupt(
        store.load_session_meta(&session_id).await,
        "SessionMeta relation",
    );
    raw.execute(
        "UPDATE session_meta SET relation_kind = 'root' WHERE session_id = 'corrupt'",
        [],
    )
    .expect("repair relation");

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
                    version: SQLITE_BLOB_ENVELOPE_VERSION,
                    compression: "Zlib".to_string(),
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
        "INSERT INTO attachment_referrer_edges (attachment_id, referrer_kind, referrer_id) VALUES ('unknown-owner', 'unknown', 'opaque')",
        [],
    )
    .expect("insert unknown owner kind");
    assert!(matches!(
        lash_core_execution::AttachmentReferrers::attachment_referrers(
            &store,
            &lash_core_execution::AttachmentId::parse("unknown-owner").unwrap()
        )
        .await,
        Err(StoreError::Incompatible { .. })
    ));

    raw.execute(
        "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
         VALUES (?1, 'dangling-artifact', 'missing-artifact-blob')",
        params![MODULE_ARTIFACT_NAMESPACE],
    )
    .expect("insert dangling artifact reference");
    let artifact_error = store
        .get_module_artifact("dangling-artifact")
        .await
        .expect_err("dangling artifact reference must fail");
    assert!(
        matches!(
            &artifact_error,
            lash_core::ArtifactStoreError::StoredDataCorrupt {
                source: lash_core::ModuleArtifactCorruption::Storage { record_kind, .. },
            } if record_kind == "artifact reference"
        ),
        "expected typed StoredDataCorrupt for dangling artifact reference, got {artifact_error:?}"
    );

    raw.execute(
        "INSERT INTO session_revisions (session_id, head_revision, head_json)
         VALUES ('corrupt', 0, '{')",
        [],
    )
    .expect("insert malformed head revision");
    raw.execute(
        "INSERT INTO session_head (session_id, head_revision) VALUES ('corrupt', 0)",
        [],
    )
    .expect("publish malformed head");
    assert_corrupt(
        store.load_session_head_meta(&session_id).await,
        "SessionHeadMeta",
    );
    assert_corrupt(
        store
            .load_session_window(&session_id, WindowSelector::Current)
            .await,
        "SessionHeadMeta",
    );

    raw.execute(
        "UPDATE session_revisions
         SET head_json = ?1, checkpoint_ref = 'missing-checkpoint-manifest'
         WHERE session_id = 'corrupt' AND head_revision = 0",
        params![
            encode_json(&SessionHeadPayload {
                session_id: SessionId::from("corrupt"),
                ..Default::default()
            })
            .expect("encode session head")
        ],
    )
    .expect("install dangling checkpoint reference");
    let dangling = store
        .load_session_window(&session_id, WindowSelector::Current)
        .await;
    assert!(
        matches!(
            &dangling,
            Err(StoreError::CheckpointComponentMissing {
                key,
                blob_ref,
            }) if key == "manifest" && blob_ref.as_str() == "missing-checkpoint-manifest"
        ),
        "a dangling checkpoint manifest must refuse the window read, got {dangling:?}"
    );
}

#[tokio::test]
async fn corrupt_graph_node_surfaces_typed_corruption_from_history_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt-graph.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open store");
    let session_id = SessionId::from("corrupt-graph");
    admit_and_seed(&store, &session_id).await;
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");
    assert!(
        raw.execute(
            "UPDATE graph_nodes SET node_json = '{' WHERE session_id = ?1",
            [session_id.as_str()],
        )
        .expect("corrupt graph node JSON")
            > 0,
        "the seeded session has graph nodes"
    );
    assert_corrupt(
        store
            .load_session_window(&session_id, WindowSelector::Current)
            .await,
        "SessionGraph",
    );
    assert_corrupt(
        store
            .load_ancestors(
                &session_id,
                lash_core_execution::store::HistoryAnchor::Head,
                lash_core_execution::store::HistoryBudget {
                    max_nodes: std::num::NonZeroU32::new(64).expect("nonzero node budget"),
                    max_bytes: std::num::NonZeroU64::new(1 << 20).expect("nonzero byte budget"),
                },
            )
            .await,
        "SessionGraph",
    );
}

#[tokio::test]
async fn closed_connection_surfaces_storage_failure_for_every_read_family() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open store");
    let session_id = SessionId::from("closed");
    store.close_for_testing().await;
    let blob_ref = BlobRef("closed".to_string());

    assert_storage_failure("lookup_session", store.lookup_session(&session_id).await);
    assert_storage_failure(
        "load_session_meta",
        store.load_session_meta(&session_id).await,
    );
    assert_storage_failure(
        "load_session_head_meta",
        store.load_session_head_meta(&session_id).await,
    );
    assert_storage_failure(
        "load_session_window",
        store
            .load_session_window(&session_id, WindowSelector::Current)
            .await,
    );
    assert_storage_failure(
        "load_ancestors",
        store
            .load_ancestors(
                &session_id,
                lash_core_execution::store::HistoryAnchor::Head,
                lash_core_execution::store::HistoryBudget {
                    max_nodes: std::num::NonZeroU32::MIN,
                    max_bytes: std::num::NonZeroU64::new(1024).expect("nonzero byte budget"),
                },
            )
            .await,
    );
    assert_storage_failure(
        "load_failure_evidence_page",
        store
            .load_failure_evidence_page(&session_id, None, std::num::NonZeroU32::MIN)
            .await,
    );
    assert_storage_failure("get_blob", store.get_blob(&blob_ref).await);
    assert_storage_failure(
        "get_typed_blob",
        store.get_typed_blob::<serde_json::Value>(&blob_ref).await,
    );
    assert_storage_failure("get_checkpoint", store.get_checkpoint(&blob_ref).await);
    assert_storage_failure(
        "AttachmentReferrers::attachment_referrers",
        lash_core_execution::AttachmentReferrers::attachment_referrers(
            store.as_ref(),
            &lash_core_execution::AttachmentId::parse("absent").unwrap(),
        )
        .await,
    );
    assert_storage_failure(
        "AttachmentRootSet::live_attachment_refs",
        lash_core_execution::AttachmentRootSet::live_attachment_refs(store.as_ref()).await,
    );
}
