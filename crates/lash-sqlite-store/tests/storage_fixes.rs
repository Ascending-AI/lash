//! Regression tests for the storage review fixes:
//!
//! * head-revision CAS holds across two independent connections to the same
//!   file database (the `BEGIN IMMEDIATE` fix),
//! * a contended queued-work admission has exactly one winner instead of a
//!   false success (the rows-affected check),
//! * a poisoned connection mutex recovers instead of bricking the store,
//! * an unsupported compatibility floor reports the recorded version and reader range,
//! * concurrent first opens do not expose an unstamped store,
//! * `gc_unreachable` never panics on a corrupt rooted manifest and keeps
//!   every blob in that conservative case.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_core_execution::compat::CompatRefusal;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use std::future::Future;
use std::sync::Arc;

use lash_core_execution::runtime::{ProcessWakeDelivery, QueuedWorkBatchDraft};
use lash_core_execution::store::RunStore as _;
use lash_core_execution::testing::store_fixtures::RuntimeStoreTestShiftExt;
use lash_core_execution::{
    AttachmentReferrers, AttachmentRootSet, LeaseOwnerIdentity, QueuedWorkStore, RuntimeCommit,
    RuntimeSessionState, SessionCatalogStore, SessionCommitStore, StoreError, StoreSchemaVerdict,
};
use lash_sqlite_store::{SqliteStore, verify_schema_at};

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

async fn sealed_shift_fence(
    store: &SqliteStore,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    executor_id: &str,
) -> lash_core_execution::store::ShiftFence {
    store
        .seal_shift_epoch_for_test(session_id, owner, executor_id, 0)
        .await
        .expect("seal shift epoch")
        .acquired()
        .expect("shift epoch sealed")
}

/// Admit `run` headed by the batch `head` under `fence`.
async fn admit(
    store: &SqliteStore,
    fence: &lash_core_execution::store::ShiftFence,
    run: &str,
    head: &lash_core_execution::BatchId,
) -> Result<Option<lash_core_execution::store::RunAdmission>, StoreError> {
    store
        .admit_run(
            &lash_core_execution::testing::store_fixtures::admit_run_request_for_test(
                fence,
                &lash_core_execution::TurnId::fixture(run),
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
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    let commit = RuntimeCommit {
        expected_head_revision,
        ..RuntimeCommit::persisted_state_for_test(&state)
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
    block_on(async {
        let store = SqliteStore::open_file_for_testing(&path)
            .await
            .expect("open catalog before the writer race");
        store
            .admit_session(
                &lash_core_execution::testing::store_fixtures::root_session_request(
                    &SessionId::from("root"),
                ),
            )
            .await
            .expect("admit session before the writer race");
    });
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let run =
        |path: std::path::PathBuf, barrier: Arc<std::sync::Barrier>, writer_id: &'static str| {
            std::thread::spawn(move || {
                block_on(async move {
                    let store = SqliteStore::open_file_for_testing(&path)
                        .await
                        .expect("open store");
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
    let store = block_on(SqliteStore::open_file_for_testing(&path)).expect("reopen store");
    let read = block_on(store.load_session_head_meta(&SessionId::from("root")))
        .expect("load")
        .expect("session present");
    assert_eq!(read.head_revision, 1);
}

fn exclusive_draft(session_id: &SessionId, text: &str) -> QueuedWorkBatchDraft {
    let process_id = ProcessId::fixture(&format!("process:{text}"));
    let sequence = 1;
    let wake = ProcessWakeDelivery {
        version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        target_session_id: session_id.clone(),
        process_id: process_id.clone(),
        sequence,
        event_type: "process.wake".to_string(),
        process_caused_by: None,
        authority: lash_core_execution::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 0,
        trace_cause: Default::default(),
    };
    lash_core_execution::runtime::process_wake_batch_draft(wake)
}

// Finding 2 (concurrent): two callers under one fence on two connections
// race to admit different runs headed by the same single ready batch. The
// admission is read-then-write, so without the unfinished-run index and the
// rows-affected check (and the `BEGIN IMMEDIATE` that serializes the read
// with the write) both could believe they won. At most one admission may
// succeed, and a successful one must actually own the batch.
#[test]
fn concurrent_admissions_never_double_own_a_batch() {
    let path = unique_db_path("admission-race");
    let batch_id = block_on(async {
        let seed = SqliteStore::open_file_for_testing(&path)
            .await
            .expect("seed store");
        seed.enqueue_queued_work(exclusive_draft(&SessionId::from("root"), "work"))
            .await
            .expect("enqueue")
            .batch_id
    });
    let fence = {
        let store = block_on(SqliteStore::open_file_for_testing(&path)).expect("admission store");
        let owner = lease_owner("session-owner");
        block_on(sealed_shift_fence(
            &store,
            &SessionId::from("root"),
            &owner,
            "concurrent-admissions-never-double-own-a-batch-executor",
        ))
    };

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let executed = |path: std::path::PathBuf,
                    fence: lash_core_execution::store::ShiftFence,
                    run: &'static str,
                    batch_id: lash_core_execution::BatchId,
                    barrier: Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            block_on(async move {
                let store = SqliteStore::open_file_for_testing(&path)
                    .await
                    .expect("open store");
                barrier.wait();
                admit(&store, &fence, run, &batch_id).await
            })
        })
    };

    let handle_a = executed(
        path.clone(),
        fence.clone(),
        "run-a",
        batch_id.clone(),
        Arc::clone(&barrier),
    );
    let handle_b = executed(path.clone(), fence, "run-b", batch_id, Arc::clone(&barrier));
    let result_a = handle_a.join().expect("thread a");
    let result_b = handle_b.join().expect("thread b");

    let mut winners = Vec::new();
    for result in [result_a, result_b] {
        match result {
            Ok(Some(admission)) => winners.push(admission),
            Ok(None) | Err(StoreError::UnfinishedRunConflict { .. } | StoreError::Contended) => {}
            Err(err) => panic!("a contended admission must resolve cleanly, got error: {err:?}"),
        }
    }
    assert_eq!(
        winners.len(),
        1,
        "exactly one run may win the single batch, got {} winners",
        winners.len()
    );
    // A successful admission really owns the batch: while its run is
    // unfinished, the batch is hidden from the user-editable pending snapshot.
    let verify = block_on(SqliteStore::open_file_for_testing(&path)).expect("verify store");
    let pending = block_on(verify.list_open_queued_work(&SessionId::from("root")))
        .expect("list pending during the winning admission");
    assert!(
        pending.is_empty(),
        "the winning admission must own its batch, hiding it from pending work"
    );
}

// Finding 7: compatibility admission reports the recorded version and this
// build's reader range without relying on the retired user_version pragma.
#[tokio::test]
async fn unsupported_compatibility_floor_reports_real_versions() {
    let path = unique_db_path("schema");
    drop(
        SqliteStore::open_file_for_testing(&path)
            .await
            .expect("provision current catalog"),
    );
    let conn = rusqlite::Connection::open(&path).expect("open raw");
    conn.execute(
        "UPDATE lash_compat SET version = 1099, min_reader = 1099",
        [],
    )
    .expect("raise the recorded reader floor");
    drop(conn);

    let found = verify_schema_at(&path).await;
    assert_eq!(
        found.verdict,
        StoreSchemaVerdict::Refused {
            refusal: CompatRefusal::ReaderFloorAbove {
                component: "sqlite-core".to_owned(),
                found: 1099,
                min_reader: 1099,
                reads: crate::sqlite_core().reads,
                writing_release: None,
            },
        }
    );
    assert!(SqliteStore::open_file_for_testing(&path).await.is_err());
}

#[test]
fn concurrent_first_open_never_observes_an_unstamped_schema() {
    let path = unique_db_path("concurrent-schema");
    let workers = 16;
    let barrier = Arc::new(std::sync::Barrier::new(workers));
    let handles = (0..workers)
        .map(|_| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                block_on(SqliteStore::open_file_for_testing(&path))
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
    let stamp: (String, i64, i64, i64) = conn
        .query_row(
            "SELECT component, version, min_reader, fleet_format FROM lash_compat WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read compatibility stamp");
    // A provisioned database records the version this build writes, the
    // oldest it reads as the floor, and the epoch an installer seeds.
    let core = crate::sqlite_core();
    assert_eq!(
        stamp,
        (
            "sqlite-core".to_owned(),
            i64::from(core.writes.max()),
            i64::from(core.reads.min()),
            i64::from(lash_core_execution::FleetFormat::seed(
                lash_core_execution::FleetFormat::writable()
            )
            .version()),
        )
    );
}

#[tokio::test]
async fn process_record_is_a_root_without_registry_liveness() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteStore::open(&dir.path().join("sessions.db"))
        .await
        .expect("open catalog");
    let request = lash_core_execution::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("unwired-process-owner"),
        relation: lash_core_execution::SessionRelation::default(),
        config: lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        )
        .into(),
        head: lash_core_execution::SessionCreationHead::Config,
    };
    store.admit_session(&request).await.expect("admit session");
    let attachment_id = lash_core_execution::AttachmentId::parse("unwired-process-attachment")
        .expect("valid attachment id");
    let intent = lash_core_execution::AttachmentWrite {
        attachment_id: attachment_id.clone(),
        claim: lash_core_execution::ReferrerClaim::unguarded(
            lash_core_execution::ArtifactReferrer::ProcessRecord(ProcessId::fixture(
                "lasting-process",
            )),
        )
        .expect("claim"),
    };
    let lash_core_execution::AttachmentWriteFence::Granted(permit) = store
        .begin_attachment_write(&(intent.clone()))
        .await
        .expect("begin process-owned write")
    else {
        panic!("a free digest must grant its writer");
    };
    store
        .complete_attachment_write(&intent, permit)
        .await
        .expect("stamp process-owned upload");

    let refs = store
        .live_attachment_refs()
        .await
        .expect("unwired GC root scan");
    assert!(refs.contains(&attachment_id));
}

#[tokio::test]
async fn plugin_state_cutover_refuses_snapshot_predecessor_without_mutation() {
    let path = unique_db_path("plugin-state-predecessor");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("provision current schema");
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE lash_compat SET version = 51, min_reader = 51", [])
        .unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    drop(conn);
    let found = verify_schema_at(&path).await;
    assert_eq!(
        found.verdict,
        StoreSchemaVerdict::Refused {
            refusal: crate::retired_core_refusal(51),
        }
    );
    assert!(SqliteStore::open_file_for_testing(&path).await.is_err());
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("SELECT version FROM lash_compat", [], |row| row.get(0))
        .unwrap();
    let after: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 51);
    assert_eq!(before, after);
}
