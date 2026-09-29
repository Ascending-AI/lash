//! Regression tests for the storage review fixes:
//!
//! * head-revision CAS holds across two independent connections to the same
//!   file database (the `BEGIN IMMEDIATE` fix),
//! * a contended queued-work admission has exactly one winner instead of a
//!   false success (the rows-affected check),
//! * a poisoned connection mutex recovers instead of bricking the store,
//! * the unsupported-schema error reports the real expected/found versions,
//! * concurrent first opens do not expose a schema-version-0 store,
//! * `gc_unreachable` never panics on a corrupt rooted manifest and keeps
//!   every blob in that conservative case.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use std::future::Future;
use std::sync::Arc;

use lash_core_execution::runtime::{ProcessWakeDelivery, QueuedWorkBatchDraft, RuntimeSubject};
use lash_core_execution::store::RootStore as _;
use lash_core_execution::testing::store_fixtures::RuntimePersistenceTestDriveExt;
use lash_core_execution::{
    AttachmentRootSet, IngressStore, LeaseOwnerIdentity, PluginState, RuntimeCommit,
    RuntimeInvocation, RuntimeSessionState, SessionCommitStore, SessionStoreFactory, StoreError,
    ToolState,
};
use lash_sqlite_store::{SqliteSessionStoreFactory, Store};

fn unique_db_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lash-storage-fixes-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("session.db")
}

fn persisted_tool_state_at_generation(generation: u64) -> ToolState {
    serde_json::from_value(serde_json::json!({
        "generation": generation,
        "tools": {}
    }))
    .expect("deserialize persisted tool state")
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

fn lease_owner(owner_id: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

async fn sealed_drive_fence(
    store: &SqliteStore,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    executor_id: &str,
) -> lash_core_execution::store::DriveFence {
    store
        .seal_drive_epoch_for_test(session_id, owner, executor_id, 0)
        .await
        .expect("seal drive epoch")
        .acquired()
        .expect("drive epoch sealed")
}

/// Admit `root` headed by the batch `head` under `fence`.
async fn admit(
    store: &SqliteStore,
    fence: &lash_core_execution::store::DriveFence,
    root: &str,
    head: &lash_core_execution::BatchId,
) -> Result<Option<lash_core_execution::store::RootAdmission>, StoreError> {
    store
        .admit_root(
            &lash_core_execution::testing::store_fixtures::admit_root_request_for_test(
                fence,
                &lash_core_execution::TurnId::from(root),
                lash_core_execution::store::AdmittedHead::Batch(head.clone()),
            ),
        )
        .await
}

fn commit_at(
    session_id: &SessionId,
    expected_head_revision: u64,
    writer_id: &str,
) -> RuntimeCommit {
    let state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    let commit = RuntimeCommit {
        expected_head_revision,
        ..RuntimeCommit::persisted_state_for_test(&state, &[])
    };
    commit
        .with_operation(lash_core_execution::OperationId::new(
            lash_core_execution::ExecutionScope::runtime_operation(format!(
                "head-cas:{session_id}:{writer_id}"
            )),
            "commit",
        ))
        .expect("build distinct head-CAS operation")
        .0
}

// Finding 1: the head-revision compare-and-set must serialize across two
// independent connections to the *same* file database. Two threads, each with
// its own connection, both read head revision 0 and then commit with
// `expected_head_revision = 0` as concurrently as a barrier can arrange.
// Under `BEGIN IMMEDIATE` the second writer blocks on the busy timeout, then
// reads the now-bumped revision and returns a clean `HeadRevisionConflict`;
// exactly one commit applies and the persisted head ends at revision 1. Under
// the old `BEGIN DEFERRED` both reads ran on a shared snapshot, letting the
// losing writer either double-apply or fail with a raw busy error instead of a
// clean conflict.
#[test]
fn head_revision_cas_holds_across_two_connections() {
    let path = unique_db_path("cas");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let run =
        |path: std::path::PathBuf, barrier: Arc<std::sync::Barrier>, writer_id: &'static str| {
            std::thread::spawn(move || {
                block_on(async move {
                    let store = SqliteStore::open(&path).await.expect("open store");
                    barrier.wait();
                    store
                        .commit_runtime_state(commit_at(&SessionId::from("root"), 0, writer_id))
                        .await
                })
            })
        };

    let handle_a = run(path.clone(), Arc::clone(&barrier), "a");
    let handle_b = run(path.clone(), Arc::clone(&barrier), "b");
    let result_a = handle_a.join().expect("thread a");
    let result_b = handle_b.join().expect("thread b");

    let winners = [&result_a, &result_b]
        .iter()
        .filter(|res| res.is_ok())
        .count();
    let conflicts = [&result_a, &result_b]
        .iter()
        .filter(|res| matches!(res, Err(StoreError::HeadRevisionConflict { .. })))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one connection may win the CAS, got a={result_a:?} b={result_b:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the loser must observe a clean HeadRevisionConflict (not a raw busy \
         error or a second success), got a={result_a:?} b={result_b:?}"
    );

    // The persisted head must reflect exactly one applied commit.
    let store = block_on(SqliteStore::open(&path)).expect("reopen store");
    let read = block_on(store.load_session())
        .expect("load")
        .expect("session present");
    assert_eq!(read.head_revision, 1);
}

// Finding 5: a checkpoint committed through the real `commit_runtime_state`
// path carries tool / plugin / execution snapshot blobs. `gc_unreachable` must
// treat that live checkpoint's child blobs as reachable and keep them, while
// still collecting genuinely orphaned blobs — and it must never panic inside
// the commit while doing so.
#[tokio::test]
async fn gc_keeps_live_committed_checkpoint_blobs() {
    let store = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("memory backend")
        .open_store()
        .await
        .expect("store");
    let orphan = store
        .put_unrooted_artifact_blob_for_testing(
            lash_sqlite_store::BlobArtifactDescriptor::checkpoint_component(),
            b"orphan-blob",
        )
        .await
        .expect("store orphan blob");

    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    state.set_tool_state_snapshot(Some(persisted_tool_state_at_generation(3)));
    state.set_plugin_state(Some(PluginState {
        plugins: Default::default(),
    }));
    state.set_execution_state_snapshot(Some(vec![0xDE, 0xAD, 0xBE, 0xEF].into()));
    store
        .admit_and_bind_session(&lash_core_execution::SessionBinding::root(
            state.session_id.clone(),
        ))
        .await
        .expect("bind session to store");
    let commit = RuntimeCommit {
        expected_head_revision: 0,
        ..RuntimeCommit::persisted_state_for_test(&state, &[])
    };
    let result = store.commit_runtime_state(commit).await.expect("commit");

    let report = store.gc_unreachable().await.expect("gc sweeps");
    assert!(
        report.deleted_blob_count >= 1,
        "the orphan blob should be collected, report={report:?}"
    );
    assert!(
        store
            .get_blob(&orphan)
            .await
            .expect("read orphan blob")
            .is_none(),
        "orphan blob must be collected"
    );

    // The live committed checkpoint manifest and every snapshot it references
    // must survive GC.
    assert!(
        store
            .get_blob(&result.checkpoint_ref)
            .await
            .expect("read checkpoint blob")
            .is_some(),
        "live checkpoint manifest must survive gc"
    );
    let manifest = store
        .get_checkpoint(&result.checkpoint_ref)
        .await
        .expect("read checkpoint")
        .expect("checkpoint manifest");
    for component in manifest.components.values() {
        let blob_ref = component
            .blob_ref()
            .expect("hydrated component carries ref");
        assert!(
            store
                .get_blob(blob_ref)
                .await
                .expect("read checkpoint child blob")
                .is_some(),
            "live checkpoint child blob {blob_ref} must survive gc"
        );
    }
}

fn exclusive_draft(session_id: &SessionId, text: &str) -> QueuedWorkBatchDraft {
    let process_id = ProcessId::fixture(&format!("process:{text}"));
    let sequence = 1;
    let wake = ProcessWakeDelivery {
        version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("wake:{text}"),
        target_session_id: SessionId::from(session_id.to_string()),
        process_id: process_id.clone(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            attribution: lash_core_execution::RuntimeAttribution::for_session(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: process_id.clone(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core_execution::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 0,
    };
    lash_core_execution::runtime::process_wake_batch_draft(wake)
}

// Finding 2 (sequential): a batch admitted to one root is not won by a
// second admission. One root takes the only ready batch; a second root headed
// by the same batch is refused while the first is unfinished.
#[tokio::test]
async fn second_admission_of_an_admitted_batch_is_not_won() {
    let store = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("memory backend")
        .open_store()
        .await
        .expect("store");
    let batch = store
        .enqueue_queued_work(exclusive_draft(&SessionId::from("root"), "work"))
        .await
        .expect("enqueue");
    let fence = sealed_drive_fence(
        &store,
        &SessionId::from("root"),
        &lease_owner("session-owner"),
        "second-admission-of-an-admitted-batch-is-not-won-executor",
    )
    .await;

    let admission_a = admit(&store, &fence, "root-a", &batch.batch_id)
        .await
        .expect("admit root a")
        .expect("root a takes the only batch");
    assert_eq!(admission_a.batch_ids(), vec![batch.batch_id.clone()]);

    let admission_b = admit(&store, &fence, "root-b", &batch.batch_id).await;
    assert!(
        matches!(admission_b, Err(StoreError::UnfinishedRootConflict { .. })),
        "a batch admitted to an unfinished root must not be admitted again, got {admission_b:?}"
    );

    // The admitted batch is hidden from the user-editable pending snapshot.
    assert!(
        store
            .list_open_queued_work(&SessionId::from("root"))
            .await
            .expect("list pending during root a's admission")
            .is_empty(),
        "the batch admitted to root a must be hidden from pending work"
    );
}

// Finding 2 (concurrent): two callers under one fence on two connections
// race to admit different roots headed by the same single ready batch. The
// admission is read-then-write, so without the unfinished-root index and the
// rows-affected check (and the `BEGIN IMMEDIATE` that serializes the read
// with the write) both could believe they won. At most one admission may
// succeed, and a successful one must actually own the batch.
#[test]
fn concurrent_admissions_never_double_own_a_batch() {
    let path = unique_db_path("admission-race");
    let batch_id = block_on(async {
        let seed = SqliteStore::open(&path).await.expect("seed store");
        seed.enqueue_queued_work(exclusive_draft(&SessionId::from("root"), "work"))
            .await
            .expect("enqueue")
            .batch_id
    });
    let fence = {
        let store = block_on(SqliteStore::open(&path)).expect("admission store");
        let owner = lease_owner("session-owner");
        block_on(sealed_drive_fence(
            &store,
            &SessionId::from("root"),
            &owner,
            "concurrent-admissions-never-double-own-a-batch-executor",
        ))
    };

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let run = |path: std::path::PathBuf,
               fence: lash_core_execution::store::DriveFence,
               root: &'static str,
               batch_id: lash_core_execution::BatchId,
               barrier: Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            block_on(async move {
                let store = SqliteStore::open(&path).await.expect("open store");
                barrier.wait();
                admit(&store, &fence, root, &batch_id).await
            })
        })
    };

    let handle_a = run(
        path.clone(),
        fence.clone(),
        "root-a",
        batch_id.clone(),
        Arc::clone(&barrier),
    );
    let handle_b = run(
        path.clone(),
        fence,
        "root-b",
        batch_id,
        Arc::clone(&barrier),
    );
    let result_a = handle_a.join().expect("thread a");
    let result_b = handle_b.join().expect("thread b");

    let mut winners = Vec::new();
    for result in [result_a, result_b] {
        match result {
            Ok(Some(admission)) => winners.push(admission),
            Ok(None) | Err(StoreError::UnfinishedRootConflict { .. } | StoreError::Contended) => {}
            Err(err) => panic!("a contended admission must resolve cleanly, got error: {err:?}"),
        }
    }
    assert_eq!(
        winners.len(),
        1,
        "exactly one root may win the single batch, got {} winners",
        winners.len()
    );
    // A successful admission really owns the batch: while its root is
    // unfinished, the batch is hidden from the user-editable pending snapshot.
    let verify = block_on(SqliteStore::open(&path)).expect("verify store");
    let pending = block_on(verify.list_open_queued_work(&SessionId::from("root")))
        .expect("list pending during the winning admission");
    assert!(
        pending.is_empty(),
        "the winning admission must own its batch, hiding it from pending work"
    );
}

// Finding 7: opening a database stamped with an unsupported schema version must
// report the actual expected and found versions, not a stale "version 1 only".
#[tokio::test]
async fn unsupported_schema_error_reports_real_versions() {
    let path = unique_db_path("schema");
    {
        let conn = rusqlite::Connection::open(&path).expect("open raw");
        // Create a user object and stamp a bogus, unsupported user_version so
        // the store's open path takes the reject branch.
        conn.execute_batch("CREATE TABLE legacy (id INTEGER); PRAGMA user_version = 1099;")
            .expect("seed legacy schema");
    }

    let message = match SqliteStore::open(&path).await {
        Ok(_) => panic!("opening an unsupported schema must fail"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains("1099"),
        "error must report the found version 1099: {message}"
    );
    assert!(
        message.contains("schema version 99"),
        "error must report the real expected version 99: {message}"
    );
    assert!(
        !message.contains("version 1 only"),
        "error must not carry the stale 'version 1 only' text: {message}"
    );
}

#[test]
fn concurrent_first_open_never_observes_version_zero_schema() {
    let path = unique_db_path("concurrent-schema");
    let workers = 16;
    let barrier = Arc::new(std::sync::Barrier::new(workers));
    let handles = (0..workers)
        .map(|_| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                block_on(SqliteStore::open(&path))
                    .map(|_| ())
                    .map_err(|err| err.to_string())
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle
            .join()
            .expect("schema-open worker")
            .expect("concurrent first open should succeed");
    }
    let conn = rusqlite::Connection::open(&path).expect("open initialized db");
    let user_version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version");
    assert_eq!(user_version, 99);
    let payload_hash_not_null: i32 = conn
        .query_row(
            "SELECT \"notnull\" FROM pragma_table_info('usage_deltas')
             WHERE name = 'payload_hash'",
            [],
            |row| row.get(0),
        )
        .expect("payload_hash column exists");
    assert_eq!(payload_hash_not_null, 1);
    let payload_encoding_version_not_null: i32 = conn
        .query_row(
            "SELECT \"notnull\" FROM pragma_table_info('usage_deltas')
             WHERE name = 'payload_encoding_version'",
            [],
            |row| row.get(0),
        )
        .expect("payload_encoding_version column exists");
    assert_eq!(payload_encoding_version_not_null, 1);
    let usage_schema: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'usage_deltas'",
            [],
            |row| row.get(0),
        )
        .expect("read usage_deltas schema");
    assert!(
        usage_schema.contains(
            "UNIQUE (session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash)"
        ),
        "usage identity uniqueness must include the payload encoding version and canonical hash: {usage_schema}"
    );
}

#[tokio::test]
async fn unwired_sqlite_factory_keeps_process_owned_intents_immortal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path().join("sessions"));
    let request = lash_core_execution::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("unwired-process-owner"),
        relation: lash_core_execution::SessionRelation::default(),
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    let store = factory.create_store(&request).await.expect("create store");
    let attachment_id = lash_core_execution::AttachmentId::parse("unwired-process-attachment")
        .expect("valid attachment id");
    let intent = lash_core_execution::AttachmentIntent {
        attachment_id: attachment_id.clone(),
        session_id: request.session_id,
        canonical_uri: "lash-attachment://unwired-process-attachment".to_string(),
        intent_at_epoch_ms: 1,
        owner: Some(lash_core_execution::AttachmentOwner::Process {
            process_id: ProcessId::fixture("missing-process"),
        }),
    };
    let lash_core_execution::AttachmentWriteFence::Granted(permit) = store
        .begin_attachment_write(intent.clone())
        .await
        .expect("begin process-owned write")
    else {
        panic!("a free digest must grant its writer");
    };
    store
        .complete_attachment_write(&intent, permit)
        .await
        .expect("stamp process-owned upload");

    let refs = factory
        .live_attachment_refs(u64::MAX)
        .await
        .expect("unwired GC root scan");
    assert!(refs.contains(&attachment_id));
}

#[tokio::test]
async fn sqlite_registry_validation_fails_gc_not_session_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join("sessions");
    let foreign_path = dir.path().join("foreign.db");
    SqliteStore::open(&foreign_path)
        .await
        .expect("create non-registry Lash database");
    let factory = SqliteSessionStoreFactory::new_with_process_registry(&sessions, &foreign_path);
    let request = lash_core_execution::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("validation-boundary"),
        relation: lash_core_execution::SessionRelation::default(),
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };

    factory
        .create_store(&request)
        .await
        .expect("ordinary session open must not probe process registry");
    let error = factory
        .live_attachment_refs(0)
        .await
        .expect_err("GC must reject a foreign process registry");
    assert!(
        error
            .to_string()
            .contains("configured database is not a Lash process registry"),
        "unexpected GC validation error: {error}"
    );
}

#[tokio::test]
async fn plugin_state_cutover_refuses_snapshot_predecessor_without_mutation() {
    let path = unique_db_path("plugin-state-predecessor");
    let store = SqliteStore::open(&path)
        .await
        .expect("provision current schema");
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA user_version = 51;").unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    drop(conn);
    let error = match SqliteStore::open(&path).await {
        Ok(_) => panic!("snapshot predecessor must be refused"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("schema version 99") && error.contains("version 51"),
        "{error}"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let after: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 51);
    assert_eq!(before, after);
}
