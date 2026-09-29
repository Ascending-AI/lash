use super::*;
use lash_core_execution::{ModuleArtifactStore, QueuedWorkStore as _, SessionCatalogStore as _};

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
    let store = crate::test_support::memory_store()
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
async fn unknown_attachment_owner_kind_refuses_with_canonical_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unknown-attachment-owner.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open store");
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
async fn unminted_process_attachment_owner_refuses_with_canonical_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unminted-process-attachment-owner.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open store");
    let raw = rusqlite::Connection::open(&path).expect("open raw connection");
    raw.execute(
        "INSERT INTO attachment_manifest
         (attachment_id, session_id, canonical_uri, intent_at_ms,
          committed_at_ms, owner_kind, owner_id)
         VALUES ('unminted-process-owner', 'unminted-process-attachment-owner',
                 'lash-attachment://unminted-process', 0, NULL, 'process', 'process-1')",
        [],
    )
    .expect("insert a process owner no registrar minted");

    let error = lash_core_execution::AttachmentManifest::list_uncommitted(&store, 0)
        .await
        .expect_err("an unminted SQLite process attachment owner must refuse");
    let expected = lash_core_execution::ProcessId::parse("process-1")
        .expect_err("a host-chosen name is not a process id")
        .to_string();
    assert!(
        matches!(
            error,
            StoreError::StoredDataCorrupt {
                record_kind: "AttachmentManifest owner",
                ref message,
            } if *message == expected
        ),
        "SQLite must return the canonical unminted-process-owner refusal, got {error:?}"
    );
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
                &lash_core_execution::ArtifactOwner::host("readonly-test"),
                module.module_ref().as_str(),
                &module.to_store_bytes().expect("encode module"),
            )
            .await
    });

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
            SqliteStore::insert_artifact_blob_conn(
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
            SqliteStore::put_typed_artifact_blob_conn(
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
        SqliteStore::open_at(
            &crate::location::DatabaseLocation::standalone_file(&path),
            StoreOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
            None,
            None,
            lash_core_execution::FleetFormat::writable_range(),
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
                QueuedWorkRead::Pending => store.list_open_queued_work(&session_id).await,
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
