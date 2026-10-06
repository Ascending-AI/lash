#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_sansio::ProcessId;
use lash_sansio::SessionId;

// No attachment_store_*_tests!: those laws certify the separate FileAttachmentStore component.
// live_replay_tests! run in tests/live_replay.rs, against the facade's PostgreSQL live replay store.
// No runtime_persistence_clock_tests!: the backend clock is PostgreSQL-owned and not controllable.
// No queued-lane resolver macro: engine pacing belongs to Restate, not a persistence store.

#[path = "blob_probe.rs"]
mod blob_probe;
lash_conformance::attachment_adoption_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = attachment_bytes(&bytes_root);
    (
        (database_fixture, bytes_root),
        Arc::new(storage.session_store_factory()),
        make_bytes,
    )
});

/// Fresh, empty attachment byte stores for the run-set laws, each a
/// filesystem store in its own directory under `run`: PostgreSQL keeps no
/// attachment bytes of its own.
/// `commit` as `run`'s final commit under `fence`: it completes every row
/// the run admitted and writes the run's terminal.
fn finishing_run(
    commit: lash_core_execution::RuntimeCommit,
    fence: &lash_core_execution::store::ShiftFence,
    run: &str,
    admission: &lash_core_execution::store::RunAdmission,
) -> lash_core_execution::RuntimeCommit {
    let run = lash_core_execution::TurnId::fixture(run);
    let mut settlement = lash_core_execution::store::IngressSettlement::new(run.clone());
    settlement
        .completed_batches
        .extend(admission.queued.as_ref().map(|queued| queued.completion()));
    settlement
        .completed_inputs
        .extend(admission.inputs.as_ref().map(|inputs| inputs.completion()));
    let mut commit = lash_core_execution::testing::store_fixtures::settling_commit_for_test(
        commit, fence, settlement,
    );
    commit.run_terminal = Some(Box::new(lash_core_execution::store::RunTerminalWrite {
        commit: lash_core_execution::store::TurnCommitId::new(run.clone(), 0),
        turn: lash_core_execution::store::PhysicalTurn::derive_turn_id(&run, 0),
        run,
        outcome: lash_core_execution::store::RunCommittedOutcome::Finished(
            lash_core_execution::facade_support::TurnFinish::AssistantMessage {
                text: String::new(),
            },
        ),
    }));
    commit
}

fn attachment_bytes(root: &tempfile::TempDir) -> lash_conformance::AttachmentBytesFactory {
    let root = root.path().to_path_buf();
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Arc::new(move || {
        let ordinal = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Arc::new(
            lash_core_execution::facade_support::FileAttachmentStore::new(
                root.join(format!("bytes-{ordinal}")),
            ),
        ) as Arc<dyn lash_core_execution::AttachmentStore>
    })
}

#[path = "conformance/admission_atomicity.rs"]
mod admission_atomicity;
#[path = "conformance/artifact_races.rs"]
mod artifact_races;
#[path = "conformance/attachment_catalog.rs"]
mod attachment_catalog;
#[path = "conformance/attachment_recovery.rs"]
mod attachment_recovery;
#[path = "conformance/generation_drain.rs"]
mod generation_drain;
#[path = "conformance/obligation_relay.rs"]
mod obligation_relay;
#[path = "conformance/occurrence_listing.rs"]
mod occurrence_listing;

use std::sync::Arc;

use lash_conformance::{
    FenceIntegrityHandles, FenceIntegrityInjector, FenceIntegrityObservation, FenceIntegrityTarget,
    GraphFactObservation, LineageConformanceHandles, LineageConformanceInjector,
    ReopenableProcessRegistry, ReopenableTriggerStore,
};
use lash_core_execution::compat::CompatRefusal;
use lash_core_execution::testing::store_fixtures::RuntimeStoreTestShiftExt as _;
use lash_core_execution::{
    AttachmentReferrers as _, DeploymentStore, ProcessExecutionEnvStore, ProcessRegistry,
    QueuedWorkStore as _, RuntimeStore, SessionCatalogStore as _, SessionCommitStore as _,
    StoreError, TriggerStore,
};
use lash_postgres_store::{PostgresStorage, PostgresStoreConfig};

#[allow(dead_code)]
mod support;

#[path = "conformance/fixture_isolation.rs"]
mod fixture_isolation;

#[path = "conformance/non_terminal_page_collation.rs"]
mod non_terminal_page_collation;
#[path = "conformance/session_delete_blob_reclaim.rs"]
mod session_delete_blob_reclaim;
#[path = "conformance/session_ingress.rs"]
mod session_ingress;
#[path = "conformance/wake_delivery.rs"]
mod wake_delivery;

use injectors::{PostgresFenceIntegrityInjector, PostgresLineageConformanceInjector};
use lash_postgres_store::testing::IsolatedDatabase;
use occurrence_listing::PostgresTriggerOccurrenceRetentionFaultInjector;
use support::{IsolatedSchema, database_url, reset};

lash_conformance::lineage_tests!({
    let Some((database_fixture, handles)) = postgres_lineage_handles().await else {
        eprintln!("skipping Postgres lineage laws: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    (database_fixture, handles)
});

fn sync_await<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
    // Execute the future on the CURRENT (multi-thread) test runtime rather than a
    // throwaway one. The sqlx pool's connections are bound to this runtime's
    // reactor; polling them from a different runtime wedges the connection (it
    // never returns to the pool), which starves the pool and surfaces as
    // PoolTimedOut. `block_in_place` lets this worker block while tokio spins up a
    // replacement, so the conformance harness keeps making progress.
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

async fn storage() -> Option<(IsolatedDatabase, PostgresStorage)> {
    let url = database_url()?;
    let database_fixture = IsolatedDatabase::create(&url).await;
    let storage = PostgresStorage::connect(database_fixture.url())
        .await
        .expect("connect postgres");
    Some((database_fixture, storage))
}

/// The storage ports of a law over `storage`, with filesystem attachment
/// bytes in a directory the caller keeps alive.
fn pg_law_stores(
    storage: &PostgresStorage,
) -> (tempfile::TempDir, Arc<dyn lash_core_execution::StoreSet>) {
    let attachments = tempfile::tempdir().expect("attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        storage,
        Arc::new(lash_core_execution::facade_support::FileAttachmentStore::new(attachments.path())),
    ));
    (attachments, stores)
}

async fn postgres_lineage_handles() -> Option<(IsolatedDatabase, LineageConformanceHandles)> {
    let (database_fixture, storage) = storage().await?;
    reset(storage.pool()).await;
    let storage = Arc::new(storage);
    let handles = LineageConformanceHandles {
        factory: Arc::new(storage.session_store_factory()),
        injector: Arc::new(PostgresLineageConformanceInjector {
            storage: Arc::clone(&storage),
        }),
    };
    Some((database_fixture, handles))
}

lash_conformance::tool_access_persistence_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres tool-access recovery: database URL is not set");
        return;
    };
    reset(storage.pool()).await;
    (database_fixture, Arc::new(storage.session_store_factory()))
});

lash_conformance::fence_integrity_tests!({
    let Some(database_url) = database_url() else {
        eprintln!("skipping Postgres fence-integrity conformance: database is not configured");
        return;
    };
    ((), move |_session_id| {
        let database_url = database_url.clone();
        async move {
            let database_fixture = IsolatedDatabase::create(&database_url).await;
            let database_url = database_fixture.url().to_owned();
            let storage = Arc::new(
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("open Postgres fence fixture"),
            );
            reset(storage.pool()).await;
            FenceIntegrityHandles {
                runtime: Arc::new(storage.store()),
                triggers: Arc::new(storage.trigger_store()),
                injector: Arc::new(PostgresFenceIntegrityInjector {
                    _database_fixture: database_fixture,
                    storage,
                }),
            }
        }
    })
});

/// Block until at least `at_least` backends are queued on `session_id`'s
/// session-execution-lease advisory lock.
///
/// Asked of `pg_locks` by the lock's own identity rather than of
/// `pg_stat_activity` by the waiter's statement *text*. The text was the one
/// spelling of the lock that existed when this test was written; FIG-3387
/// gave the six call sites that take a seed-`0` text lock one named statement,
/// and a text match would have gone silently to zero waiters. The key is what
/// the lock is: `pg_advisory_xact_lock(bigint)` splits its argument into
/// `classid` (high 32 bits) and `objid` (low 32), with `objsubid = 1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_graph_node_primary_key_is_global_when_configured() {
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres global node-id schema test: database is not configured");
        return;
    };
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
         FROM pg_constraint
         WHERE conrelid = 'lash_graph_nodes'::regclass
           AND contype = 'p'",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read graph-node primary key");

    assert_eq!(definition, "PRIMARY KEY (node_id)");
}

lash_conformance::store_recovery_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres store-recovery laws: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    reset(storage.pool()).await;
    let database_url = database_fixture.url().to_owned();
    (
        database_fixture,
        move |_session_id: &str| {
            let database_url = database_url.clone();
            let storage = sync_await(async move {
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("construct fresh Postgres store-recovery pool")
            });
            Arc::new(storage.store()) as Arc<dyn RuntimeStore>
        },
        lash_conformance::StoreRecoveryLeaseTiming::Realtime,
    )
});

lash_conformance::checkpoint_component_reopen_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres checkpoint-component recovery: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let database_url = _database_fixture.url().to_owned();
    (_database_fixture, move || {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("construct post-write Postgres checkpoint pool")
        });
        Arc::new(storage.store()) as Arc<dyn RuntimeStore>
    })
});

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_runtime_turn_receipt_rejects_half_populated_append_identity_when_configured() {
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres receipt-schema test: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let error = sqlx::query(
        "INSERT INTO lash_runtime_turn_commits (
            session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
            request_identity_hash, failure_evidence, change_seq
         ) VALUES ('half-identity', 'half-identity', 'hash', '{}', 0, 'request-hash', FALSE, 1)",
    )
    .execute(storage.pool())
    .await
    .expect_err("a half-populated append identity must violate the schema CHECK");
    assert!(
        error.to_string().contains("check constraint"),
        "Postgres must report the schema CHECK: {error}"
    );
}

lash_conformance::append_head_switch_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres append-receipt conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let pool = storage.pool().clone();
    (
        _database_fixture,
        Arc::new(storage.store()) as Arc<dyn RuntimeStore>,
        move |leaf_node_id: lash_core_execution::NodeId| async move {
            sqlx::query(
                "WITH recorded AS (
                     INSERT INTO lash_session_revisions (session_id, head_revision, leaf_node_id, checkpoint_ref, head_json)
                     SELECT session_id, head_revision + 1, $1, checkpoint_ref, head_json
                     FROM lash_session_head JOIN lash_session_revisions USING (session_id, head_revision)
                     WHERE session_id = 'root' RETURNING session_id, head_revision
                 ) UPDATE lash_session_head AS head SET head_revision = recorded.head_revision
                   FROM recorded WHERE head.session_id = recorded.session_id",
            )
            .bind(leaf_node_id.into_inner())
            .execute(&pool)
            .await
            .expect("switch Postgres active branch");
        },
    )
});

lash_conformance::append_tombstone_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres tombstoned-leaf conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let pool = storage.pool().clone();
    (
        _database_fixture,
        Arc::new(storage.store()) as Arc<dyn RuntimeStore>,
        move |node_id: lash_core_execution::NodeId| async move {
            sqlx::query("UPDATE lash_graph_nodes SET tombstoned = TRUE WHERE node_id = $1")
                .bind(node_id.into_inner())
                .execute(&pool)
                .await
                .expect("tombstone Postgres old leaf");
        },
    )
});

lash_conformance::append_receipt_rewrite_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres old-format receipt conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    storage
        .store()
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&SessionId::from(
                "root",
            )),
        )
        .await
        .expect("admit old-format receipt run");
    let pool = storage.pool().clone();
    (
        _database_fixture,
        Arc::new(storage.store()) as Arc<dyn RuntimeStore>,
        move || async move {
            sqlx::query(
                "UPDATE lash_runtime_turn_commits
                 SET result_json = ((result_json::jsonb
                     - 'committed_leaf_node_id'
                     - 'receipt_replayed')::text)
                 WHERE turn_id LIKE '%old-format-append-receipt%'
                   AND turn_id NOT LIKE '%old-format-append-receipt-seed%'",
            )
            .execute(&pool)
            .await
            .expect("install raw pre-upgrade Postgres receipt fixture");
        },
    )
});

lash_conformance::artifact_store_reopenable_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres artifact-store conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    let database_url = database_fixture.url().to_owned();
    (database_fixture, move || {
        let storage = Arc::clone(&storage);
        let database_url = database_url.clone();
        sync_await(async move {
            reset(storage.pool()).await;
            let open_storage = PostgresStorage::connect(&database_url)
                .await
                .expect("open first Postgres artifact pool");
            let open = lash_conformance::fused_artifact_store::ArtifactStoreHandles {
                artifacts: Arc::new(open_storage.lashlang_artifact_store())
                    as Arc<dyn lash_core::ModuleArtifactStore>,
                process_env: Arc::new(open_storage.process_env_store())
                    as Arc<dyn ProcessExecutionEnvStore>,
                turn_preludes: Arc::new(open_storage.process_env_store())
                    as Arc<dyn lash_core::TurnPreludeStore>,
            };
            let reopen_url = database_url.clone();
            lash_conformance::fused_artifact_store::ReopenableArtifactStore {
                open,
                reopen: Arc::new(move || {
                    let reopen_url = reopen_url.clone();
                    let reopened = sync_await(async move {
                        PostgresStorage::connect(&reopen_url)
                            .await
                            .expect("construct post-write Postgres artifact pool")
                    });
                    lash_conformance::fused_artifact_store::ArtifactStoreHandles {
                        artifacts: Arc::new(reopened.lashlang_artifact_store())
                            as Arc<dyn lash_core::ModuleArtifactStore>,
                        process_env: Arc::new(reopened.process_env_store())
                            as Arc<dyn ProcessExecutionEnvStore>,
                        turn_preludes: Arc::new(reopened.process_env_store())
                            as Arc<dyn lash_core::TurnPreludeStore>,
                    }
                }),
            }
        })
    })
});

#[path = "conformance/artifact_retention.rs"]
mod artifact_retention;

/// Corrupt the live checkpoint manifest so the mark phase cannot decode the
/// root it must follow, giving the sweep a real failure to report.
struct PostgresCorruptRootedManifest {
    storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl lash_conformance::StoreMaintenanceFaultInjector for PostgresCorruptRootedManifest {
    async fn break_gc_scope(&self, _session_id: &SessionId) {
        let corrupted = sqlx::query(
            "UPDATE lash_blobs SET content = '\\xffffffff'::bytea
             WHERE hash IN (SELECT checkpoint_ref FROM lash_session_head JOIN lash_session_revisions USING (session_id, head_revision)
                            WHERE checkpoint_ref IS NOT NULL)",
        )
        .execute(self.storage.pool())
        .await
        .expect("corrupt the rooted checkpoint manifest")
        .rows_affected();
        assert!(
            corrupted >= 1,
            "the fault must corrupt at least one rooted manifest"
        );
    }
}

lash_conformance::store_maintenance_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres maintenance-outcome conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    let make_storage = Arc::clone(&storage);
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = attachment_bytes(&bytes_root);
    (
        (database_fixture, bytes_root),
        "postgres",
        move || {
            let storage = Arc::clone(&make_storage);
            sync_await(async move {
                reset(storage.pool()).await;
                Arc::new(storage.session_store_factory()) as Arc<dyn DeploymentStore>
            })
        },
        move || make_bytes(),
    )
});

lash_conformance::store_maintenance_fault_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres maintenance-outcome conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    let make_storage = Arc::clone(&storage);
    (
        database_fixture,
        "postgres",
        move || {
            let storage = Arc::clone(&make_storage);
            sync_await(async move {
                reset(storage.pool()).await;
                Arc::new(storage.session_store_factory()) as Arc<dyn DeploymentStore>
            })
        },
        Arc::new(PostgresCorruptRootedManifest { storage }),
    )
});

lash_conformance::fresh_session_admission_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres fresh-admission conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    (_database_fixture, move |_session_id: &str| {
        Arc::new(storage.store()) as Arc<dyn RuntimeStore>
    })
});

lash_conformance::observer_intent_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres fork-observer intent conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_fixture, attachments),
        lash_conformance::recording_backend_over(stores),
    )
});

lash_conformance::queue_observation_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres queue observation conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_fixture, attachments),
        lash_conformance::recording_backend_over(stores),
    )
});

lash_conformance::session_graph_append_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres session-graph append branch-liveness conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    (
        _database_fixture,
        Arc::new(storage.session_store_factory()) as Arc<dyn DeploymentStore>,
    )
});

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_wake_enqueue_serializes_with_consumption_when_configured() {
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres wake enqueue interleaving test: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let factory = storage.session_store_factory();
    let session_id = "wake-source-lock-target";
    factory
        .admit_session(&lash_core_execution::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::fixture(session_id.to_string()),
            relation: lash_core_execution::SessionRelation::Root,
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
                lash_core_execution::MaxToolCalls::new(1024),
            )
            .into(),
            head: lash_core_execution::SessionCreationHead::Config,
        })
        .await
        .expect("admit source-lock target");
    let store = Arc::new(factory.clone()) as Arc<dyn RuntimeStore>;
    let wake = lash_core_execution::ProcessWakeDelivery {
        version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        target_session_id: SessionId::fixture(session_id.to_string()),
        process_id: ProcessId::fixture("wake-source-lock-process"),
        sequence: 1,
        event_type: "producer.wake".to_string(),
        process_caused_by: None,
        authority: lash_core_execution::QueuedWorkAuthority::default(),
        input: "wake".to_string(),
        created_at_ms: lash_core_execution::ClockWallTime::timestamp_ms(
            &lash_core_execution::facade_support::SystemClock,
        ),
        trace_cause: Default::default(),
    };
    let draft = lash_core_execution::runtime::process_wake_batch_draft(wake.clone());
    let first = store
        .enqueue_queued_work(draft.clone())
        .await
        .expect("enqueue original wake");
    let owner = lash_core_execution::LeaseOwnerIdentity::opaque("wake-source-lock", "test");
    let lease = store
        .seal_shift_epoch_for_test(
            &SessionId::from(session_id),
            &owner,
            "wake-executor-1",
            60_000,
        )
        .await
        .expect("seal target shift")
        .acquired()
        .expect("shift sealed");
    let admission = lash_core_execution::testing::store_fixtures::admit_run_for_test(
        &store,
        &lease,
        &lash_core_execution::TurnId::from("wake-source-run"),
        lash_core_execution::store::AdmittedHead::Batch(first.batch_id.clone()),
    )
    .await
    .expect("admit source-lock wake")
    .expect("source-lock wake admission");
    assert_eq!(
        admission.batch_ids(),
        vec![first.batch_id.clone()],
        "the original wake heads the lane"
    );

    let source_key = draft.source_key.as_deref().expect("wake source key");
    let mut source_blocker = storage.pool().begin().await.expect("begin source blocker");
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
             hashtextextended(
                 length($1)::TEXT || ':' || $1 || length($2)::TEXT || ':' || $2,
                 0
             )
         )",
    )
    .bind(session_id)
    .bind(source_key)
    .execute(&mut *source_blocker)
    .await
    .expect("take source-only advisory lock");
    let redelivery_store = Arc::clone(&store);
    let redelivery_draft = draft.clone();
    let redelivery =
        tokio::spawn(async move { redelivery_store.enqueue_queued_work(redelivery_draft).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !redelivery.is_finished(),
        "enqueue must block on the independently-held source lock"
    );
    source_blocker
        .rollback()
        .await
        .expect("release source-only enqueue blocker");
    redelivery
        .await
        .expect("join source-blocked redelivery")
        .expect("redelivery returns the live batch");

    let mut completion_source_blocker = storage
        .pool()
        .begin()
        .await
        .expect("begin completion source blocker");
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
             hashtextextended(
                 length($1)::TEXT || ':' || $1 || length($2)::TEXT || ':' || $2,
                 0
             )
         )",
    )
    .bind(session_id)
    .bind(source_key)
    .execute(&mut *completion_source_blocker)
    .await
    .expect("take completion source-only advisory lock");
    let completion_store = Arc::clone(&store);
    let completion = tokio::spawn(async move {
        let state = lash_core_execution::RuntimeSessionState {
            session_id: SessionId::fixture(session_id.to_string()),
            ..lash_core_execution::RuntimeSessionState::new(
                lash_core_execution::SessionPolicy::new(
                    lash_core_execution::TurnBudget::Unbounded,
                    lash_core_execution::MaxToolCalls::new(1024),
                ),
            )
        };
        completion_store
            .commit_runtime_state(finishing_run(
                lash_core_execution::RuntimeCommit::persisted_state_for_test(&state),
                &lease,
                "wake-source-run",
                &admission,
            ))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !completion.is_finished(),
        "consumption must block on the independently-held source lock"
    );
    completion_source_blocker
        .rollback()
        .await
        .expect("release source-only completion blocker");
    completion
        .await
        .expect("join wake consumption")
        .expect("consume wake after source lock release");
    assert!(
        store
            .list_queued_work(&SessionId::from(session_id))
            .await
            .expect("list queue after forced interleaving")
            .iter()
            .all(|batch| batch.source_key.as_deref() != draft.source_key.as_deref()),
        "forced evidence-check/drain/live-check interleaving must not recreate the wake"
    );
    // Until vacuum the delivered wake's tombstone answers its redelivery.
    let answered = store
        .enqueue_queued_work(draft.clone())
        .await
        .expect("a late redelivery answers the delivered tombstone");
    assert_eq!(
        answered.terminal.as_ref().map(|terminal| terminal.cause),
        Some(lash_core_execution::store::IngressTerminalCause::Delivered),
        "the late redelivery is the delivered wake: {answered:?}"
    );
    store
        .vacuum(&SessionId::from(session_id))
        .await
        .expect("vacuum the delivered tombstone");
    let late_redelivery = store
        .enqueue_queued_work(draft.clone())
        .await
        .expect_err("a vacuumed wake at the receiver floor is a typed rewind");
    assert!(matches!(
        late_redelivery,
        lash_core_execution::StoreError::ProcessWakeSequenceRewound {
            sequence: 1,
            allocation_floor: 1,
            ..
        }
    ));
    assert!(
        store
            .list_queued_work(&SessionId::from(session_id))
            .await
            .expect("list queue after late redelivery")
            .iter()
            .all(|batch| batch.source_key.as_deref() != draft.source_key.as_deref())
    );

    let bounded_config = PostgresStoreConfig {
        lock_timeout: Some(std::time::Duration::from_millis(50)),
        ..PostgresStoreConfig::default()
    };
    let bounded_storage = PostgresStorage::connect_with(_database_fixture.url(), bounded_config)
        .await
        .expect("connect storage with short lock timeout");
    let bounded_store = bounded_storage.store();
    let mut timeout_wake = wake;
    timeout_wake.sequence = 2;
    let timeout_draft = lash_core_execution::runtime::process_wake_batch_draft(timeout_wake);
    let timeout_retry_draft = timeout_draft.clone();
    let timeout_source_key = timeout_draft
        .source_key
        .as_deref()
        .expect("timeout wake source key");
    let mut timeout_blocker = storage
        .pool()
        .begin()
        .await
        .expect("begin timeout source blocker");
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
             hashtextextended(
                 length($1)::TEXT || ':' || $1 || length($2)::TEXT || ':' || $2,
                 0
             )
         )",
    )
    .bind(session_id)
    .bind(timeout_source_key)
    .execute(&mut *timeout_blocker)
    .await
    .expect("take timeout source-only advisory lock");
    let timeout_error = bounded_store
        .enqueue_queued_work(timeout_draft)
        .await
        .expect_err("source lock wait must be bounded");
    assert!(
        matches!(timeout_error, lash_core_execution::StoreError::Contended),
        "source lock timeout must surface as retryable contention: {timeout_error}"
    );
    timeout_blocker
        .rollback()
        .await
        .expect("release timeout source blocker");
    let second = store
        .enqueue_queued_work(timeout_retry_draft)
        .await
        .expect("enqueue second sequence after source lock release");
    let second_owner =
        lash_core_execution::LeaseOwnerIdentity::opaque("wake-source-lock-second", "test");
    let second_lease = store
        .seal_shift_epoch_for_test(
            &SessionId::from(session_id),
            &second_owner,
            "wake-executor-2",
            60_000,
        )
        .await
        .expect("claim target for second sequence")
        .acquired()
        .expect("second-sequence target lease");
    let second_admission = lash_core_execution::testing::store_fixtures::admit_run_for_test(
        &store,
        &second_lease,
        &lash_core_execution::TurnId::from("wake-source-second-run"),
        lash_core_execution::store::AdmittedHead::Batch(second.batch_id.clone()),
    )
    .await
    .expect("admit second wake sequence")
    .expect("second wake sequence admission");
    assert_eq!(
        second_admission.batch_ids(),
        vec![second.batch_id.clone()],
        "the second sequence heads the lane"
    );
    let view = lash_core_execution::store::SessionStore::new(
        Arc::clone(&store),
        SessionId::from(session_id),
    )
    .expect("valid wake target session id");
    let state = lash_core_execution::store::load_session_window_state(
        &view,
        lash_core_execution::store::WindowSelector::Current,
    )
    .await
    .expect("load target state before second wake settlement")
    .expect("persisted target state")
    .state;
    store
        .commit_runtime_state(finishing_run(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state),
            &second_lease,
            "wake-source-second-run",
            &second_admission,
        ))
        .await
        .expect("consume second wake sequence");
    let fence_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_wake_redelivery_fences WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(storage.pool())
    .await
    .expect("count receiver high-water rows");
    assert_eq!(
        fence_rows, 1,
        "two consumed sequences for one process must occupy one allocation-fence row"
    );
    let allocation_floor: i64 = sqlx::query_scalar(
        "SELECT allocation_floor FROM lash_wake_redelivery_fences
         WHERE session_id = $1 AND process_id = $2",
    )
    .bind(session_id)
    .bind(ProcessId::fixture("wake-source-lock-process").as_str())
    .fetch_one(storage.pool())
    .await
    .expect("read receiver allocation floor");
    assert_eq!(allocation_floor, 2);
    sqlx::query(
        "INSERT INTO lash_wake_allocation_floors (
            target_session_id, process_id, allocation_floor
         ) VALUES ($1, $2, $3)",
    )
    .bind(session_id)
    .bind(ProcessId::fixture("wake-source-lock-process").as_str())
    .bind(2_i64)
    .execute(storage.pool())
    .await
    .expect("seed matching sender allocation floor");
    factory
        .delete_session(&SessionId::from(session_id))
        .await
        .expect("delete high-water target session");
    let fence_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_wake_redelivery_fences WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(storage.pool())
    .await
    .expect("count receiver high-water rows after session delete");
    assert_eq!(
        fence_rows, 0,
        "session deletion must remove its receiver allocation-fence rows"
    );
    let sender_floor_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_wake_allocation_floors
         WHERE target_session_id = $1",
    )
    .bind(session_id)
    .fetch_one(storage.pool())
    .await
    .expect("count sender allocation floors after session delete");
    assert_eq!(
        sender_floor_rows, 0,
        "session deletion must remove sender and receiver floors together"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_turn_commit_stamps_use_injected_store_clock_when_configured() {
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres injected commit clock regression: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    const SESSION_ID: &str = "postgres-injected-commit-clock";
    const TURN_ID: &str = "postgres-injected-clock-turn";
    const NOW_MS: u64 = 1_234_567;
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(NOW_MS));
    let factory = storage.store().with_clock(clock);
    factory
        .admit_session(&lash_core_execution::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::fixture(SESSION_ID.to_string()),
            relation: lash_core_execution::SessionRelation::default(),
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
                lash_core_execution::MaxToolCalls::new(1024),
            )
            .into(),
            head: lash_core_execution::SessionCreationHead::Config,
        })
        .await
        .expect("admit clocked Postgres session");
    let store = factory.clone();
    let clock_intent = lash_core_execution::AttachmentWrite {
        attachment_id: lash_core_execution::AttachmentId::parse("postgres-clock-attachment")
            .expect("valid attachment id"),
        claim: lash_core_execution::ReferrerClaim::unguarded(
            lash_core_execution::ArtifactReferrer::Session(SessionId::from(SESSION_ID)),
        )
        .expect("session claim"),
    };
    let lash_core_execution::AttachmentWriteFence::Granted(clock_permit) = store
        .begin_attachment_write(&clock_intent)
        .await
        .expect("begin turn-owned write")
    else {
        panic!("a free digest must grant its writer");
    };
    store
        .complete_attachment_write(&clock_intent, clock_permit)
        .await
        .expect("stamp turn-owned upload");
    let owner =
        lash_core_execution::LeaseOwnerIdentity::opaque("clock-test", "clock-test-incarnation");
    let _lease = store
        .seal_shift_epoch_for_test(
            &SessionId::from(SESSION_ID),
            &owner,
            "clock-executor",
            60_000,
        )
        .await
        .expect("claim clock test lease")
        .acquired()
        .expect("clock test lease acquired");
    let state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::fixture(SESSION_ID.to_string()),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    };
    // A mid-turn commit: the stamps are the subject, and a final commit
    // would also need its run's admitted cancellation snapshot (FIG-4848).
    let operation = lash_core_execution::OperationId::turn(SESSION_ID, TURN_ID, "checkpoint");
    let operation_key = operation.storage_key().expect("canonical operation key");
    let (commit, _) = lash_core_execution::RuntimeCommit::persisted_state_for_test(&state)
        .with_committed_attachments(vec![clock_intent.attachment_id.clone()])
        .with_operation(operation)
        .expect("stamp clock test commit");
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit with injected clock");

    let manifest_stamp: i64 = sqlx::query_scalar(
        "SELECT written_at_ms FROM lash_attachment_uploads WHERE attachment_id = $1",
    )
    .bind(clock_intent.attachment_id.as_str())
    .fetch_one(storage.pool())
    .await
    .expect("read manifest commit stamp");
    let turn_stamp: i64 = sqlx::query_scalar(
        "SELECT committed_at_ms FROM lash_runtime_turn_commits
         WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(SESSION_ID)
    .bind(operation_key)
    .fetch_one(storage.pool())
    .await
    .expect("read turn commit stamp");
    assert_eq!(manifest_stamp as u64, NOW_MS);
    assert_eq!(turn_stamp as u64, NOW_MS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_from_pool_rejects_unstamped_existing_schema_when_configured() {
    let Some(url) = database_url() else {
        eprintln!(
            "skipping Postgres unstamped-schema gate test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let scratch = IsolatedSchema::provision(&url).await;
    let pool = scratch.pool.clone();
    sqlx::query("DELETE FROM lash_schema_versions WHERE component = 'lash-postgres-store'")
        .execute(&pool)
        .await
        .expect("remove component version stamp");

    let result = PostgresStorage::from_pool(pool.clone()).await;
    scratch.cleanup().await;
    assert!(matches!(
        result,
        Err(StoreError::Incompatible {
            refusal: CompatRefusal::Unstamped { .. }
        })
    ));
}

lash_conformance::process_registry_reopenable_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres process conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    (database_fixture, move |_: &str| {
        let storage = Arc::clone(&storage);
        sync_await(async move {
            reset(storage.pool()).await;
            let open = Arc::new(storage.process_registry())
                as Arc<dyn lash_core_execution::ConformanceProcessRegistry>;
            let reopen = Arc::new(storage.process_registry())
                as Arc<dyn lash_core_execution::ConformanceProcessRegistry>;
            ReopenableProcessRegistry { open, reopen }
        })
    })
});

lash_conformance::process_change_horizon_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres prune-horizon conformance: database URL is not set");
        return;
    };
    reset(storage.pool()).await;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    (database_fixture, registry)
});

lash_conformance::process_projection_repair_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres leased replay repair: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    reset(storage.pool()).await;
    let pool = storage.pool().clone();
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    (
        database_fixture,
        registry,
        move |stale: lash_core_execution::ProcessRecord| async move {
            let changed =
                sqlx::query("UPDATE lash_processes SET record_json = $2 WHERE process_id = $1")
                    .bind(stale.id.as_str())
                    .bind(serde_json::to_string(&stale).expect("encode stale process projection"))
                    .execute(&pool)
                    .await
                    .expect("corrupt Postgres process projection")
                    .rows_affected();
            assert_eq!(changed, 1);
        },
    )
});

lash_conformance::process_trigger_retention_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres process-trigger retention conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    (database_fixture, move || {
        let storage = Arc::clone(&storage);
        async move {
            reset(storage.pool()).await;
            lash_conformance::ProcessTriggerRetentionHandles {
                registry: Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>,
                triggers: Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>,
                sessions: Arc::new(storage.store())
                    as Arc<dyn lash_core_execution::DeploymentStore>,
                deliveries: storage
                    .obligation_ledger(lash_core_execution::store::ObligationKind::TriggerDelivery),
                process_starts: storage
                    .obligation_ledger(lash_core_execution::store::ObligationKind::ProcessStart),
                process_env: Arc::new(storage.process_env_store())
                    as Arc<dyn lash_core_execution::ProcessExecutionEnvStore>,
            }
        }
    })
});

lash_conformance::tool_intent_retention_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres tool-intent retention conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    (database_fixture, move || {
        let storage = Arc::clone(&storage);
        async move {
            reset(storage.pool()).await;
            let handles =
                |storage: &PostgresStorage| lash_conformance::ToolIntentRetentionHandles {
                    registry: Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>,
                    sessions: Arc::new(storage.store())
                        as Arc<dyn lash_core_execution::DeploymentStore>,
                };
            let open = handles(&storage);
            let reopen: lash_conformance::ToolIntentRetentionReopen = Arc::new(move || {
                let storage = Arc::clone(&storage);
                Box::pin(async move { handles(&storage) })
            });
            lash_conformance::ToolIntentRetentionFixture { open, reopen }
        }
    })
});

lash_conformance::trigger_occurrence_tombstone_retention_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres trigger-occurrence tombstone retention laws: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    (
        database_fixture,
        move |clock: Arc<dyn lash_core_execution::Clock>| {
            let storage = Arc::clone(&storage);
            async move {
                reset(storage.pool()).await;
                Arc::new(storage.trigger_store().with_clock(clock)) as Arc<dyn TriggerStore>
            }
        },
    )
});

lash_conformance::store_contract_state_machine_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres store-contract properties: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    (database_fixture, "postgres", move |_, _session_id| {
        let storage = Arc::clone(&storage);
        async move {
            reset(storage.pool()).await;
            lash_conformance::StoreContractHandles {
                registry: Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>,
                runtime: Arc::new(storage.store()) as Arc<dyn RuntimeStore>,
            }
        }
    })
});

lash_conformance::runtime_persistence_state_machine_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres runtime-persistence properties: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = attachment_bytes(&bytes_root);
    ((database_fixture, bytes_root), "postgres", move |_| {
        let storage = Arc::clone(&storage);
        let attachments = make_bytes();
        async move {
            reset(storage.pool()).await;
            lash_conformance::RuntimePersistenceStateMachineHandles::create(
                Arc::new(storage.store()),
                attachments,
            )
            .await
            .expect("create Postgres runtime-persistence property handles")
        }
    })
});

lash_conformance::session_graph_state_machine_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres session-graph properties: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    (database_fixture, "postgres", move |_| {
        let storage = Arc::clone(&storage);
        async move {
            reset(storage.pool()).await;
            Arc::new(storage.session_store_factory())
                as Arc<dyn lash_core_execution::store::ConformanceDeployment>
        }
    })
});

lash_conformance::process_continuation_store_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres continuation conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let process_storage = Arc::new(storage.process_registry());
    let registry = Arc::clone(&process_storage) as Arc<dyn lash_core_execution::ProcessRegistry>;
    let store = process_storage as Arc<dyn lash_core_execution::ProcessContinuationStore>;
    (database_fixture, registry, store)
});

#[test]
fn trigger_subscription_owner_filter_is_pushed_down() {
    lash_conformance::trigger_subscription_owner_filter_is_pushed_down(
        "PostgreSQL",
        lash_postgres_store::testing::trigger_subscription_list_sql,
    );
}

lash_conformance::trigger_store_reopenable_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres trigger conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    (database_fixture, move || {
        let storage = Arc::clone(&storage);
        sync_await(async move {
            reset(storage.pool()).await;
            let open = Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>;
            let reopen = Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>;
            ReopenableTriggerStore { open, reopen }
        })
    })
});

lash_conformance::trigger_retention_fault_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres trigger retention fault laws: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let pool = storage.pool().clone();
    let store = Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>;
    let fault = Arc::new(PostgresTriggerOccurrenceRetentionFaultInjector { pool });
    (database_fixture, store, fault)
});

#[path = "conformance/process_retention.rs"]
mod process_retention;
include!("conformance/append_identity.rs");
#[path = "conformance/injectors.rs"]
mod injectors;
lash_conformance::session_read_view_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres read-session conformance: database URL is not set");
        return;
    };
    reset(storage.pool()).await;
    (
        _database_fixture,
        Arc::new(storage.session_store_factory()) as Arc<dyn DeploymentStore>,
    )
});

lash_conformance::retention_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        return;
    };
    (database_fixture, Arc::new(storage.session_store_factory()))
});

mod driver_turn_ownership {}

// The ownership law where every await suspends and every resumption replays
// the handler's journal from its start (FIG-4514): a run replayed after its
// terminal-checkpoint follow-on committed names that follow-on's effects as
// its first execution did, so the shift ends.
mod driver_turn_ownership_under_replay {}

mod run_control {}

mod session_history {
    use super::*;
    use lash_core_execution::store::ConformanceDeployment;

    async fn catalog() -> Option<(
        IsolatedDatabase,
        Arc<dyn ConformanceDeployment>,
        PostgresStorage,
    )> {
        let (lock, storage) = storage().await?;
        reset(storage.pool()).await;
        let store = Arc::new(storage.store()) as Arc<dyn ConformanceDeployment>;
        Some((lock, store, storage))
    }

    lash_conformance::turn_commit_outcome_tests!({
        let Some((lock, store, _storage)) = catalog().await else {
            return;
        };
        (lock, store)
    });

    #[tokio::test]
    async fn unread_turn_terminals_survive_retention() {
        let (_lock, store, _storage) = catalog()
            .await
            .expect("PostgreSQL law requires its isolated database");
        lash_conformance::unread_terminals_survive_retention(store).await;
    }

    #[tokio::test]
    async fn terminal_feed_is_ordered_and_replay_stable() {
        let (_lock, store, _storage) = catalog()
            .await
            .expect("PostgreSQL law requires its isolated database");
        lash_conformance::terminal_feed_is_ordered_and_replay_stable(store).await;
    }

    #[tokio::test]
    async fn window_is_frame_bounded() {
        let Some((_lock, store, _storage)) = catalog().await else {
            return;
        };
        lash_conformance::history_window_is_frame_bounded(store).await;
    }

    #[tokio::test]
    async fn pages_are_bounded_and_pinned() {
        let Some((_lock, store, _storage)) = catalog().await else {
            return;
        };
        lash_conformance::history_pages_are_bounded_and_pinned(store).await;
    }

    #[tokio::test]
    async fn fork_respects_ceiling() {
        let Some((_lock, store, _storage)) = catalog().await else {
            return;
        };
        lash_conformance::history_fork_respects_ceiling(store).await;
    }

    #[tokio::test]
    async fn inflated_fork_ceiling_cannot_expose_post_fork_source_nodes() {
        let Some((_lock, store, _storage)) = catalog().await else {
            return;
        };
        lash_conformance::inflated_fork_ceiling_cannot_expose_post_fork_source_nodes(store).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn history_selection_and_confirmation_share_one_snapshot() {
        let Some((_lock, store, _storage)) = catalog().await else {
            return;
        };
        lash_conformance::history_selection_and_confirmation_share_one_snapshot(store).await;
    }

    #[tokio::test]
    async fn graph_generation_overflow_rolls_back_every_write() {
        let Some((_lock, store, _storage)) = catalog().await else {
            return;
        };
        lash_conformance::graph_generation_overflow_rolls_back_every_write(store).await;
    }

    #[tokio::test]
    async fn a_later_frame_open_cannot_rescue_earlier_root_nodes() {
        let Some((_lock, store, _storage)) = catalog().await else {
            return;
        };
        lash_conformance::a_later_frame_open_cannot_rescue_earlier_root_nodes(store).await;
    }

    #[tokio::test]
    async fn window_rejects_corrupt_anchors() {
        let Some((_lock, _store, storage)) = catalog().await else {
            return;
        };
        lash_conformance::history_window_rejects_corrupt_anchors(|_| {
            let storage = storage.clone();
            async move {
                reset(storage.pool()).await;
                Arc::new(storage.store()) as Arc<dyn ConformanceDeployment>
            }
        })
        .await;
    }
}

mod vm_broker {}

mod frame_open {}

mod bound_trigger_duplicate {}

#[tokio::test]
async fn fenced_process_and_trigger_registration_stays_typed() {
    let Some(url) = database_url() else {
        return;
    };
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("open older writer");
    // An epoch past this build's writable range: a newer release finalized.
    let newer = lash_core_execution::FleetFormat::writable().max() + 1;
    lash_postgres_store::testing::finalize_fleet_epoch(storage.pool(), newer)
        .await
        .expect("finalize newer fleet format");
    lash_conformance::fenced_process_and_trigger_registration_stays_typed(
        Arc::new(storage.process_registry()),
        Arc::new(storage.trigger_store()),
    )
    .await;
    for table in [
        "lash_processes",
        "lash_process_events",
        "lash_trigger_subscriptions",
        "lash_trigger_mutation_receipts",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(storage.pool())
            .await
            .expect("count rows");
        assert_eq!(count, 0, "fenced writer added rows to {table}");
    }
    storage.pool().close().await;
}

lash_conformance::attachment_referrer_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = attachment_bytes(&bytes_root);
    let pool = storage.pool().clone();
    let insert_edge: lash_conformance::InsertAttachmentEdge = Arc::new(move |id, kind, key| {
        let pool = pool.clone();
        Box::pin(async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
            sqlx::query("ALTER TABLE lash_attachment_referrer_edges DROP CONSTRAINT IF EXISTS ck_attachment_referrer_edges_kind").execute(&mut *tx).await.map_err(|error| StoreError::Backend(error.to_string()))?;
            sqlx::query("INSERT INTO lash_attachment_referrer_edges (attachment_id, referrer_kind, referrer_id) VALUES ($1, $2, $3)").bind(id.as_str()).bind(kind).bind(key).execute(&mut *tx).await.map_err(|error| StoreError::Backend(error.to_string()))?;
            sqlx::query(&format!("ALTER TABLE lash_attachment_referrer_edges ADD CONSTRAINT ck_attachment_referrer_edges_kind CHECK ({}) NOT VALID", lash_core_execution::ArtifactReferrerKind::predicate_sql("referrer_kind", lash_core_execution::ArtifactReferrerKind::holds_attachments))).execute(&mut *tx).await.map_err(|error| StoreError::Backend(error.to_string()))?;
            tx.commit()
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))
        })
    });
    let handles = lash_conformance::AttachmentReferrerHandles {
        factory: Arc::new(storage.store()),
        cleanup: storage.artifact_cleanup(),
        bytes: make_bytes,
        insert_edge,
    };
    ((database_fixture, bytes_root), handles)
});

lash_conformance::checkpoint_profile_tests!({
    let Some((guard, storage)) = storage().await else {
        eprintln!(
            "PENDING: PostgreSQL identity profile conformance needs LASH_POSTGRES_DATABASE_URL"
        );
        return;
    };
    reset(storage.pool()).await;
    let stores = vec![
        Arc::new(storage.store()) as Arc<dyn RuntimeStore>,
        Arc::new(storage.store()),
    ];
    (guard, stores)
});

#[tokio::test]
async fn nested_process_arguments_reject_forged_aliases_and_try_later_union_arms() {
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("PENDING: PostgreSQL service not configured");
        return;
    };
    reset(storage.pool()).await;
    lash_lashlang_runtime::testing::nested_process_arguments_reject_forged_aliases_and_try_later_union_arms(Arc::new(storage.lashlang_artifact_store())).await;
}

lash_conformance::process_prune_start_staging_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres environment-start conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let env_store = Arc::new(storage.process_env_store()) as Arc<dyn ProcessExecutionEnvStore>;
    (database_fixture, registry, env_store)
});

mod worker_recovery {}

#[tokio::test]
async fn a_stale_fence_receipt_replay_leaves_the_store_byte_identical() {
    let Some((_database_fixture, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let factory =
        Arc::new(storage.store()) as Arc<dyn lash_core_execution::store::ConformanceDeployment>;
    lash_conformance::a_stale_fence_receipt_replay_leaves_the_store_byte_identical(
        factory,
        || async {
            let pool = storage.pool();
            let tables: Vec<String> = sqlx::query_scalar(
                "SELECT tablename::text FROM pg_tables WHERE schemaname = current_schema() ORDER BY tablename",
            )
            .fetch_all(pool)
            .await
            .expect("read complete table census");
            let mut snapshot = Vec::new();
            for table in tables {
                let mut rows: Vec<String> =
                    sqlx::query_scalar(&format!("SELECT to_jsonb(t)::text FROM \"{table}\" AS t"))
                        .fetch_all(pool)
                        .await
                        .expect("read table snapshot");
                rows.sort();
                snapshot.push((table, rows.join("\n")));
            }
            snapshot
        },
    )
    .await;
}

mod session_commands {}

#[tokio::test]
async fn neutral_tool_receipts_are_first_writers_and_retire_with_their_scope() {
    use lash::tracing::TraceToolOwner;
    use lash_core::store::{ToolCompletionReceipt, ToolRequestReceipt, WaitReceiptStore as _};
    let (_database, storage) = storage().await.expect("hermetic PostgreSQL");
    let store = storage.session_store_factory();
    let owners = [
        TraceToolOwner::Turn {
            session_id: "neutral-tool".into(),
            turn_id: "turn".into(),
        },
        TraceToolOwner::Operation {
            session_id: "neutral-tool".into(),
            operation_id: "operation".into(),
        },
        TraceToolOwner::Process {
            process_id: ProcessId::fixture("neutral-tool-process"),
        },
    ];
    let expected_receipts = owners.len();
    for (index, owner) in owners.into_iter().enumerate() {
        let request = ToolRequestReceipt {
            owner,
            request_key: format!("neutral-tool:{index}"),
            payload_digest: "request".into(),
            payload: serde_json::json!({"original":true}),
            scope: None,
            context: Default::default(),
            requested_at_ms: 1,
        };
        assert!(store.record_tool_request(&request).await.unwrap().changed);
        assert_eq!(
            store
                .tool_request_receipt(&request.request_key)
                .await
                .unwrap(),
            Some(request.clone())
        );
        let mut offered = request.clone();
        offered.requested_at_ms = 100;
        let reused = store.record_tool_request(&offered).await.unwrap();
        assert_eq!(reused.record, request);
        assert!(reused.permit().is_none());
        offered.owner = TraceToolOwner::Process {
            process_id: ProcessId::fixture("foreign-tool-owner"),
        };
        assert!(matches!(
            store.record_tool_request(&offered).await,
            Err(StoreError::ToolRequestConflict { .. })
        ));
        let completion = ToolCompletionReceipt {
            owner: request.owner.clone(),
            request_key: request.request_key.clone(),
            payload_digest: request.payload_digest.clone(),
            result: serde_json::json!({"usage":"recorded"}),
            intent_outcomes: serde_json::json!([]),
            completed_at_ms: 2,
        };
        assert!(
            store
                .record_tool_completion(&completion)
                .await
                .unwrap()
                .changed
        );
        assert!(
            store
                .record_tool_completion(&completion)
                .await
                .unwrap()
                .permit()
                .is_none()
        );
        store
            .retire_observation_receipts(&request.owner_key().unwrap(), 3)
            .await
            .unwrap();
    }
    let report = store
        .reclaim_retained_evidence(lash_core::store::RetentionBound {
            committed_before_epoch_ms: 4,
            turn_watermark: lash_core::store::TurnProjectionWatermark::NoProjector,
        })
        .await
        .unwrap();
    assert_eq!(report.removed_receipt_count, expected_receipts);
}
