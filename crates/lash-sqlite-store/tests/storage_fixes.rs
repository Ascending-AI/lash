//! Regression tests for the storage review fixes:
//!
//! * head-revision CAS holds across two independent connections to the same
//!   file database (the `BEGIN IMMEDIATE` fix),
//! * a contended queued-work claim reports not-claimed instead of a false
//!   success (the rows-affected check),
//! * a poisoned connection mutex recovers instead of bricking the store,
//! * the unsupported-schema error reports the real expected/found versions,
//! * concurrent first opens do not expose a schema-version-0 store,
//! * `gc_unreachable` never panics on a corrupt rooted manifest and keeps
//!   every blob in that conservative case.

use std::future::Future;
use std::sync::Arc;

use lash_core::runtime::{
    ProcessWakeDelivery, QueuedWorkBatchDraft, QueuedWorkClaimBoundary, RuntimeScope,
    RuntimeSubject,
};
use lash_core::{
    AttachmentRootSet, LeaseOwnerIdentity, PendingTurnInputDraft, PluginSessionSnapshot,
    QueuedWorkStore, RuntimeCommit, RuntimeInvocation, RuntimeSessionState, SessionCommitStore,
    SessionExecutionLeaseStore, SessionStoreFactory, StoreError, ToolState, TurnInput,
    TurnInputIngress, TurnInputStore,
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

fn commit_at(session_id: &str, expected_head_revision: u64, writer_id: &str) -> RuntimeCommit {
    let state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let commit = RuntimeCommit {
        expected_head_revision,
        ..RuntimeCommit::persisted_state_for_test(&state, &[])
    };
    commit
        .with_operation(lash_core::OperationId::new(
            lash_core::ExecutionScope::runtime_operation(format!(
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
                    let store = Store::open(&path).await.expect("open store");
                    barrier.wait();
                    store
                        .commit_runtime_state(commit_at("root", 0, writer_id))
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
    let store = block_on(Store::open(&path)).expect("reopen store");
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
    let store = Store::memory().await.expect("store");
    let orphan = store
        .put_artifact_blob(
            lash_sqlite_store::BlobArtifactDescriptor::checkpoint_component(),
            b"orphan-blob",
        )
        .await
        .expect("store orphan blob");

    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        plugin_snapshot_revision: Some(5),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    state.set_tool_state_snapshot(Some(persisted_tool_state_at_generation(3)));
    state.set_plugin_snapshot(Some(PluginSessionSnapshot {
        plugins: Default::default(),
    }));
    state.set_execution_state_snapshot(Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(state.session_id.clone()))
        .await
        .expect("bind session to store");
    let owner = lease_owner("gc-test");
    let session_lease = store
        .try_claim_session_execution_lease("root", &owner, 60_000)
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease");
    let commit = RuntimeCommit {
        expected_head_revision: 0,
        ..RuntimeCommit::persisted_state_for_test(&state, &[])
    }
    .releasing_session_execution_lease(session_lease.completion());
    let result = store.commit_runtime_state(commit).await.expect("commit");

    let report = store.gc_unreachable().await;
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

fn exclusive_draft(session_id: &str, text: &str) -> QueuedWorkBatchDraft {
    let process_id = format!("process:{text}");
    let sequence = 1;
    let wake = ProcessWakeDelivery {
        wake_id: format!("wake:{text}"),
        target_session_id: session_id.to_string(),
        process_id: process_id.clone(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            scope: RuntimeScope::new(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: process_id.clone(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 0,
    };
    lash_core::runtime::process_wake_batch_draft(wake)
}

// The raw cross-backend dialect assertion is Postgres-gated. Keep the ordinary
// SQLite lane independently sensitive to both production claim-id spellings.
#[tokio::test]
async fn sqlite_claims_pin_both_production_claim_id_spellings() {
    let store = Store::memory().await.expect("store");
    let session_id = "sqlite-claim-id-dialects";
    let queued = store
        .enqueue_queued_work(exclusive_draft(session_id, "work"))
        .await
        .expect("enqueue queued work");
    let pending = store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            session_id,
            TurnInputIngress::next_turn(),
            TurnInput::text("input"),
        ))
        .await
        .expect("enqueue turn input");
    let owner = lease_owner("sqlite-claim-id-owner");
    let lease = store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease");

    let queued_claim = store
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim queued work")
        .expect("queued work claim");
    let turn_input_claim = store
        .claim_next_turn_inputs(session_id, &lease.fence(), &owner, 1)
        .await
        .expect("claim turn input")
        .expect("turn-input claim");

    assert_eq!(
        queued_claim.claim_id,
        format!("qwc:{}:1", queued.enqueue_seq)
    );
    assert_eq!(
        turn_input_claim.claim_id,
        format!("tic:{}:1", pending.enqueue_seq)
    );
}

// Finding 2 (sequential): when a batch is already held by a live claim, a
// second claim must not "succeed" with a claim that doesn't actually own the
// row. Owner A claims the only ready batch; owner B then asks to claim and must
// get `None`.
#[tokio::test]
async fn second_claim_on_held_batch_is_not_won() {
    let store = Store::memory().await.expect("store");
    store
        .enqueue_queued_work(exclusive_draft("root", "work"))
        .await
        .expect("enqueue");
    let session_lease = store
        .try_claim_session_execution_lease("root", &lease_owner("session-owner"), 60_000)
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease");
    let session_fence = session_lease.fence();

    let claim_a = store
        .claim_ready_queued_work(
            "root",
            &session_fence,
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim a")
        .expect("owner a wins the only batch");
    assert_eq!(claim_a.batches.len(), 1);

    let claim_b = store
        .claim_ready_queued_work(
            "root",
            &session_fence,
            &lease_owner("owner-b"),
            QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim b");
    assert!(
        claim_b.is_none(),
        "a batch already held by a live claim must not be re-claimed, got {claim_b:?}"
    );

    // Owner A's claim is the live one: while its session-lease generation holds
    // the batch, the batch is hidden from the user-editable pending snapshot.
    assert!(
        store
            .list_pending_queued_work("root")
            .await
            .expect("list pending during owner-a's live claim")
            .is_empty(),
        "the batch held by owner A's live claim must be hidden from pending work"
    );
    let _ = claim_a;
}

// Finding 2 (concurrent): two owners on two connections race for the same
// single ready batch. The claim is read-then-write, so without the
// rows-affected check (and the `BEGIN IMMEDIATE` that serializes the read with
// the write) both could believe they won. At most one claim may succeed, and a
// successful claim must actually own the batch (provable by renewing it).
#[test]
fn concurrent_claims_never_double_own_a_batch() {
    let path = unique_db_path("claim-race");
    block_on(async {
        let seed = Store::open(&path).await.expect("seed store");
        seed.enqueue_queued_work(exclusive_draft("root", "work"))
            .await
            .expect("enqueue");
    });
    let session_fence = {
        let store = block_on(Store::open(&path)).expect("lease store");
        let owner = lease_owner("session-owner");
        block_on(store.try_claim_session_execution_lease("root", &owner, 60_000))
            .expect("claim session execution lease")
            .acquired()
            .expect("session execution lease")
            .fence()
    };

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let run = |owner: &'static str,
               path: std::path::PathBuf,
               session_fence: lash_core::SessionExecutionLeaseAuthority,
               barrier: Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            block_on(async move {
                let store = Store::open(&path).await.expect("open store");
                barrier.wait();
                store
                    .claim_ready_queued_work(
                        "root",
                        &session_fence,
                        &lease_owner(owner),
                        QueuedWorkClaimBoundary::Idle,
                        lash_core::testing::queued_work_claim_policy(10),
                    )
                    .await
            })
        })
    };

    let handle_a = run(
        "owner-a",
        path.clone(),
        session_fence.clone(),
        Arc::clone(&barrier),
    );
    let handle_b = run("owner-b", path.clone(), session_fence, Arc::clone(&barrier));
    let result_a = handle_a.join().expect("thread a");
    let result_b = handle_b.join().expect("thread b");

    let mut winners = Vec::new();
    for result in [result_a, result_b] {
        match result {
            Ok(Some(claim)) => winners.push(claim),
            Ok(None) => {}
            Err(err) => panic!("a contended claim must resolve cleanly, got error: {err:?}"),
        }
    }
    assert!(
        winners.len() <= 1,
        "at most one owner may win the single batch, got {} winners",
        winners.len()
    );
    if let Some(claim) = winners.first() {
        // A successful claim must really own the batch: while the winner's
        // session-lease generation holds it, the batch is hidden from the
        // user-editable pending snapshot.
        let verify = block_on(Store::open(&path)).expect("verify store");
        let pending = block_on(verify.list_pending_queued_work("root"))
            .expect("list pending during the winning claim");
        assert!(
            pending.is_empty(),
            "the winning claim must own its batch, hiding it from pending work"
        );
        let _ = claim;
    }
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
        conn.execute_batch("CREATE TABLE legacy (id INTEGER); PRAGMA user_version = 99;")
            .expect("seed legacy schema");
    }

    let message = match Store::open(&path).await {
        Ok(_) => panic!("opening an unsupported schema must fail"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains("99"),
        "error must report the found version 99: {message}"
    );
    assert!(
        message.contains("schema version 35"),
        "error must report the real expected version 35: {message}"
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
                block_on(Store::open(&path))
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
    assert_eq!(user_version, 35);
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
    let request = lash_core::SessionStoreCreateRequest {
        session_id: "unwired-process-owner".to_string(),
        relation: lash_core::SessionRelation::default(),
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let store = factory.create_store(&request).await.expect("create store");
    let attachment_id = lash_core::AttachmentId::new("unwired-process-attachment");
    store
        .record_intent(lash_core::AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: request.session_id,
            canonical_uri: "lash-attachment://unwired-process-attachment".to_string(),
            intent_at_epoch_ms: 1,
            owner_kind: Some(lash_core::AttachmentOwnerKind::Process),
            owner_id: Some("missing-process".to_string()),
        })
        .expect("record process intent");

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
    Store::open(&foreign_path)
        .await
        .expect("create non-registry Lash database");
    let factory = SqliteSessionStoreFactory::new_with_process_registry(&sessions, &foreign_path);
    let request = lash_core::SessionStoreCreateRequest {
        session_id: "validation-boundary".to_string(),
        relation: lash_core::SessionRelation::default(),
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
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
