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
// No live_replay_tests!: live replay is an in-process cache, not PostgreSQL-backed storage.
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

/// Fresh, empty attachment byte stores for the root-set laws, each a
/// filesystem store in its own directory under `root`: PostgreSQL keeps no
/// attachment bytes of its own.
/// `commit` as `root`'s final commit under `fence`: it completes every row
/// the root admitted and writes the root's terminal.
fn finishing_root(
    commit: lash_core_execution::RuntimeCommit,
    fence: &lash_core_execution::store::DriveFence,
    root: &str,
    admission: &lash_core_execution::store::RootAdmission,
) -> lash_core_execution::RuntimeCommit {
    let root = lash_core_execution::TurnId::from(root);
    let mut settlement = lash_core_execution::store::IngressSettlement::new(root.clone());
    settlement
        .completed_batches
        .extend(admission.queued.as_ref().map(|queued| queued.completion()));
    settlement
        .completed_inputs
        .extend(admission.inputs.as_ref().map(|inputs| inputs.completion()));
    let mut commit = lash_core_execution::testing::store_fixtures::settling_commit_for_test(
        commit, fence, settlement,
    );
    commit.root_terminal = Some(Box::new(lash_core_execution::store::RootTerminalWrite {
        commit: lash_core_execution::store::TurnCommitId::new(root.clone(), 0),
        turn: lash_core_execution::store::PhysicalTurn::derive_turn_id(&root, 0),
        root,
        outcome: lash_core_execution::store::RootCommittedOutcome::Finished(
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
    ReopenableProcessRegistry, ReopenableRuntimeStore, ReopenableTriggerStore,
};
use lash_core_execution::compat::CompatRefusal;
use lash_core_execution::testing::store_fixtures::RuntimeStoreTestDriveExt as _;
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
#[path = "conformance/session_close.rs"]
mod session_close;
#[path = "conformance/session_delete_blob_reclaim.rs"]
mod session_delete_blob_reclaim;
#[path = "conformance/session_ingress.rs"]
mod session_ingress;
#[path = "conformance/usage_accounting.rs"]
mod usage_accounting;
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
    // Drive the future on the CURRENT (multi-thread) test runtime rather than a
    // throwaway one. The sqlx pool's connections are bound to this runtime's
    // reactor; polling them from a different runtime wedges the connection (it
    // never returns to the pool), which starves the pool and surfaces as
    // PoolTimedOut. `block_in_place` lets this worker block while tokio spins up a
    // replacement, so the conformance harness keeps making progress.
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// The promise authority a storage law's turn-control protocol runs through.
///
/// PostgreSQL journals no effects (ADR 0104): a deployment's promises are its
/// Restate engine's, so the authority is the engine's own deployment effect
/// host on the in-process server double, minting, settling and reading
/// durable waits through the double's endpoint. What the law certifies is
/// the PostgreSQL rows; the guard keeps the double alive until the law
/// finishes.
async fn promise_authority() -> (
    lash_restate_test::RestateTestBackend,
    Arc<dyn lash_core_execution::EffectHost>,
) {
    let backend =
        lash_restate_test::backend(restate_seed(), lash_restate_test::ServerConfig::default())
            .await
            .expect("boot the promise authority's Restate server double");
    let host: Arc<dyn lash_core_execution::EffectHost> = backend.restate().restate_effect_host();
    (backend, host)
}

/// The seed of a fixture's server double: `LASH_RESTATE_TEST_SEED` replays
/// one, otherwise each fixture draws a fresh one.
fn restate_seed() -> u64 {
    if let Some(seed) = std::env::var("LASH_RESTATE_TEST_SEED")
        .ok()
        .and_then(|seed| seed.parse::<u64>().ok())
    {
        return seed;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    (nanos & u128::from(u64::MAX)) as u64 ^ NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// A law attempt, repacked as one of the double's handler attempts: it
/// reports what it observed through its own channel, so the handler's job
/// ends when the attempt ends.
fn handler_attempt(
    attempt: lash_conformance::ConformanceTurnAttempt,
) -> lash_restate_test::HandlerAttempt {
    Arc::new(move |scoped| {
        let attempt = Arc::clone(&attempt);
        Box::pin(async move {
            attempt(scoped).await;
        })
    })
}

/// `ConformanceTurnRunner` over [`RestateTestBackend::run_in_handler`]: every
/// turn a law drives runs inside a handler of the double's deployment, on the
/// handler-scoped controller a Restate tier actually lends its turns, instead
/// of on a host scoped from the calling task.
///
/// [`RestateTestBackend::run_in_handler`]: lash_restate_test::RestateTestBackend::run_in_handler
struct DoubleTurnRunner {
    backend: lash_restate_test::RestateTestBackend,
    /// The invocations a crash left open, by the scope they run: the law's
    /// next turn of that scope is the double's redelivery of the invocation.
    open: std::sync::Mutex<std::collections::HashMap<String, OpenTurn>>,
}

/// An invocation whose execution a crash killed, left open for the double to
/// redeliver.
struct OpenTurn {
    /// Hands the redelivered execution the law's next attempt of the scope.
    next: tokio::sync::watch::Sender<Option<lash_conformance::ConformanceTurnAttempt>>,
    /// How the redelivered execution's attempt ended.
    ends: tokio::sync::mpsc::UnboundedReceiver<lash_conformance::ConformanceTurnEnd>,
    /// The invocation's call: it returns once a redelivered execution
    /// settled that attempt.
    call: tokio::task::JoinHandle<Result<(), String>>,
}

impl DoubleTurnRunner {
    fn shared(
        backend: lash_restate_test::RestateTestBackend,
    ) -> Arc<dyn lash_conformance::ConformanceTurnRunner> {
        Arc::new(Self {
            backend,
            open: std::sync::Mutex::default(),
        })
    }

    fn open_turns(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, OpenTurn>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The key an open invocation is held under: its admitted scope.
fn open_turn_key(admitted: &lash_core::AdmittedScope) -> String {
    format!("{:?}", admitted.scope())
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for DoubleTurnRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        let open = self.open_turns().remove(&open_turn_key(&admitted));
        let Some(open) = open else {
            self.backend
                .run_in_handler(admitted, handler_attempt(attempt))
                .await
                .unwrap_or_else(|error| {
                    panic!("the law's turn did not run in its handler: {error}")
                });
            return;
        };
        let OpenTurn {
            next,
            mut ends,
            mut call,
        } = open;
        next.send_replace(Some(attempt));
        let settled = tokio::select! {
            biased;
            end = ends.recv() => end,
            ran = &mut call => {
                panic!("the crashed turn's invocation ended ({ran:?}) before its redelivered attempt did")
            }
        };
        // An attempt that aborted leaves the invocation open, as an aborted
        // turn's invocation stays open on Restate; a settled one completes it.
        if settled == Some(lash_conformance::ConformanceTurnEnd::Settled) {
            call.await
                .expect("the open invocation's call task")
                .unwrap_or_else(|error| {
                    panic!("the law's crashed turn did not recover in its redelivery: {error}")
                });
        }
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.backend
            .run_crashed_then_redriven(
                admitted,
                handler_attempt(crashing),
                handler_attempt(redrive),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("the law's crashed turn did not redrive in its handler: {error}")
            });
    }

    async fn run_turn_until_crash(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
        crash: lash_conformance::ConformanceCrash,
    ) {
        // Inside the handler the crash kills the attempt where it stands: its
        // future is dropped mid-poll. The execution then fails retryably as
        // Restate's invocation of a dead deployment does, once the law has
        // queued its next attempt of this scope, and the double's retry
        // replays the journal the killed attempt left into that attempt. A
        // fresh invocation would start an empty journal instead, which the
        // drive's seal answers as a lost substrate (ADR 0105 L-S8), not as
        // the recovery of a crashed turn.
        let key = open_turn_key(&admitted);
        assert!(
            !self.open_turns().contains_key(&key),
            "a crash of `{key}` while an earlier crash of it is still open"
        );
        let (next, queued) =
            tokio::sync::watch::channel::<Option<lash_conformance::ConformanceTurnAttempt>>(None);
        let crashing: lash_restate_test::HandlerAttempt = {
            let crash = crash.clone();
            let queued = queued.clone();
            Arc::new(move |scoped| {
                let attempt = Arc::clone(&attempt);
                let crash = crash.clone();
                let mut queued = queued.clone();
                Box::pin(async move {
                    // An execution that starts after the crash fired (the
                    // killed one was suspended or closed) is dead as it
                    // starts.
                    if !crash.has_fired() {
                        tokio::select! {
                            biased;
                            () = crash.fired() => {}
                            end = attempt(scoped) => {
                                panic!("the crashing attempt ended ({end:?}) before its crash fired")
                            }
                        }
                    }
                    if queued.wait_for(Option::is_some).await.is_err() {
                        // The law never runs the scope again.
                        std::future::pending::<()>().await;
                    }
                    panic!("the conformance crash killed the attempt")
                })
            })
        };
        let (ended, ends) = tokio::sync::mpsc::unbounded_channel();
        let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let redelivered: lash_restate_test::HandlerAttempt = Arc::new(move |scoped| {
            let attempt = queued.borrow().clone();
            let ended = ended.clone();
            let aborted = Arc::clone(&aborted);
            Box::pin(async move {
                let attempt = attempt.expect(
                    "the double redelivers a crashed turn only once its next attempt is queued",
                );
                // The attempt aborted in an earlier execution: the invocation
                // stays open with nothing left to run.
                if !aborted.load(std::sync::atomic::Ordering::SeqCst) {
                    let end = attempt(scoped).await;
                    let _ = ended.send(end);
                    if end == lash_conformance::ConformanceTurnEnd::Settled {
                        return;
                    }
                    aborted.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                std::future::pending::<()>().await;
            })
        });
        let backend = self.backend.clone();
        let mut call = tokio::spawn(async move {
            backend
                .run_crashed_then_redriven(admitted, crashing, redelivered)
                .await
        });
        tokio::select! {
            biased;
            () = crash.fired() => {}
            result = &mut call => {
                panic!("the crashing turn's handler ended ({result:?}) before its crash fired")
            }
        }
        self.open_turns().insert(key, OpenTurn { next, ends, call });
    }

    /// Process segments run in the double's process workflow: the worker is
    /// installed there, and the runtime's own port only observes the
    /// registry that workflow writes terminals into.
    fn process_work(
        &self,
        watched: lash_core_execution::WatchedRegistry,
        worker: lash_core_worker::DurableProcessWorker,
    ) -> lash_core_execution::ProcessWorkWiring {
        self.backend.install_process_worker(worker);
        let port = Arc::new(lash_core_execution::NoProcessWork::new(&watched));
        lash_core_execution::ProcessWorkWiring::new(watched, port)
    }
}

/// A backend for a law whose turns must run inside a Restate handler:
/// lash-restate's engine on the in-process server double, its engine stores
/// decorated into `storage`'s PostgreSQL store set, so the effects a handler
/// executes land on the store under test. Returns the law's stores, the
/// engine's deployment effect host, and the handler-bound turn runner.
async fn double_law_backend(
    storage: &PostgresStorage,
) -> (
    (tempfile::TempDir, lash_restate_test::RestateTestBackend),
    Arc<dyn lash_core_execution::StoreSet>,
    Arc<dyn lash_core_execution::EffectHost>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
) {
    let (attachments, stores) = pg_law_stores(storage);
    let engine_stores = Arc::clone(&stores);
    let backend = lash_restate_test::backend_with(
        restate_seed(),
        lash_restate_test::ServerConfig::default(),
        move |_| engine_stores,
    )
    .await
    .expect("boot the law's Restate double over PostgreSQL stores");
    let host: Arc<dyn lash_core_execution::EffectHost> = backend.restate().restate_effect_host();
    let runner = DoubleTurnRunner::shared(backend.clone());
    ((attachments, backend), stores, host, runner)
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

lash_conformance::runtime_persistence_reopenable_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    // One reset per law: a law that opens several sessions (the factory
    // laws) admits each through `make`, and the catalog must keep them all.
    reset(storage.pool()).await;
    drop(storage);
    let database_url = database_fixture.url().to_owned();
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(10_000));
    let lease_clock = Arc::clone(&clock);
    let (promise_guard, effect_host) = promise_authority().await;
    (
        (database_fixture, promise_guard),
        move |session_id: &str| {
            let effect_host = Arc::clone(&effect_host);
            let database_url = database_url.clone();
            let clock = Arc::clone(&clock);
            let session_id = SessionId::from(session_id.to_string());
            sync_await(async move {
                let open_storage = PostgresStorage::connect(&database_url)
                    .await
                    .expect("open first Postgres conformance pool");
                let reopen_storage = PostgresStorage::connect(&database_url)
                    .await
                    .expect("open independent Postgres conformance pool");
                let request = lash_core_execution::SessionStoreCreateRequest {
                    owning_process_id: None,
                    pending_observer_intents: Vec::new(),
                    session_id,
                    relation: lash_core_execution::SessionRelation::Root,
                    config: lash_core_execution::SessionPolicy::new(
                        lash_core_execution::TurnBudget::Unbounded,
                    )
                    .into(),
                    head: lash_core_execution::SessionCreationHead::CommittedByCreator,
                };
                let open_factory = open_storage
                    .session_store_factory()
                    .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>)
                    .with_lease_clock_for_testing(
                        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>
                    );
                let reopen_factory = reopen_storage
                    .session_store_factory()
                    .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>)
                    .with_lease_clock_for_testing(clock as Arc<dyn lash_core_execution::Clock>);
                open_factory
                    .admit_session(&request)
                    .await
                    .expect("admit Postgres conformance session");
                let open = Arc::new(open_factory) as Arc<dyn RuntimeStore>;
                let reopen = Arc::new(reopen_factory) as Arc<dyn RuntimeStore>;
                ReopenableRuntimeStore {
                    open,
                    reopen,
                    effect_host: Arc::clone(&effect_host),
                }
            })
        },
        lash_conformance::RuntimePersistenceLeaseTiming::controlled(move |ms| {
            lease_clock.advance(ms)
        }),
    )
});

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
            request_identity_hash, failure_evidence
         ) VALUES ('half-identity', 'half-identity', 'hash', '{}', 0, 'request-hash', FALSE)",
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
                "UPDATE lash_sessions
                 SET leaf_node_id = $1, head_revision = head_revision + 1
                 WHERE session_id = 'root'",
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
        .expect("admit old-format receipt root");
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
                    }
                }),
            }
        })
    })
});

lash_conformance::artifact_referrer_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres artifact-referrer conformance: LASH_POSTGRES_DATABASE_URL is not set"
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
                    }
                }),
            }
        })
    })
});

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
             WHERE hash IN (SELECT checkpoint_ref FROM lash_sessions
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

lash_conformance::session_store_factory_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres session-store-factory conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    let make_storage = Arc::clone(&storage);
    let make = move || {
        let storage = Arc::clone(&make_storage);
        sync_await(async move {
            reset(storage.pool()).await;
            Arc::new(storage.session_store_factory())
                as Arc<dyn lash_core_execution::store::ConformanceDeployment>
        })
    };
    let attachments = Arc::new(tempfile::tempdir().expect("attachment directory"));
    let attached_storage = Arc::clone(&storage);
    let attached_root = Arc::clone(&attachments);
    let make_attached = move || {
        let storage = Arc::clone(&attached_storage);
        let root = attached_root.path().join(uuid::Uuid::new_v4().to_string());
        sync_await(async move {
            reset(storage.pool()).await;
            (
                Arc::new(storage.session_store_factory())
                    as Arc<dyn lash_core_execution::store::ConformanceDeployment>,
                Arc::new(lash_core_execution::facade_support::FileAttachmentStore::new(root))
                    as Arc<dyn lash_core_execution::AttachmentStore>,
            )
        })
    };
    let (promise_guard, effect_host) = promise_authority().await;
    (
        (_database_fixture, attachments, promise_guard),
        make,
        make_attached,
        effect_host,
    )
});

// The settlement laws run a facade runtime over a fresh backend per law: the
// Restate double's engine is built over this test's PostgreSQL store set, so
// the runtime takes its durable ports from PostgreSQL while the guard keeps
// the database, attachment and backend lifetimes.
lash_conformance::session_config_settlement_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres config-settlement conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let ((attachments, double), _stores, _host, _runner) = double_law_backend(&storage).await;
    let make = {
        let double = double.clone();
        move || {
            let double = double.clone();
            async move { double.lash_backend() }
        }
    };
    ((database_fixture, attachments, double), make)
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
            session_id: SessionId::from(session_id.to_string()),
            relation: lash_core_execution::SessionRelation::Root,
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
            )
            .into(),
            head: lash_core_execution::SessionCreationHead::CommittedByCreator,
        })
        .await
        .expect("admit source-lock target");
    let store = Arc::new(factory.clone()) as Arc<dyn RuntimeStore>;
    let wake = lash_core_execution::ProcessWakeDelivery {
        version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: "wake:source-lock".to_string(),
        target_session_id: SessionId::from(session_id.to_string()),
        process_id: ProcessId::fixture("wake-source-lock-process"),
        sequence: 1,
        event_type: "producer.wake".to_string(),
        event_invocation: lash_core_execution::RuntimeInvocation::effect(
            lash_core_execution::EffectAddress::new(
                lash_core_execution::ExecutionScope::process(ProcessId::fixture(
                    "wake-source-lock-process",
                )),
                "wake-source-lock",
            )
            .expect("valid wake effect address"),
            lash_core_execution::RuntimeAttribution::for_session(session_id),
            "wake-source-lock",
        ),
        process_caused_by: None,
        authority: lash_core_execution::QueuedWorkAuthority::default(),
        input: "wake".to_string(),
        created_at_ms: lash_core_execution::ClockWallTime::timestamp_ms(
            &lash_core_execution::facade_support::SystemClock,
        ),
    };
    let draft = lash_core_execution::runtime::process_wake_batch_draft(wake.clone());
    let first = store
        .enqueue_queued_work(draft.clone())
        .await
        .expect("enqueue original wake");
    let owner = lash_core_execution::LeaseOwnerIdentity::opaque("wake-source-lock", "test");
    let lease = store
        .seal_drive_epoch_for_test(
            &SessionId::from(session_id),
            &owner,
            "wake-executor-1",
            60_000,
        )
        .await
        .expect("seal target drive")
        .acquired()
        .expect("drive sealed");
    let admission = lash_core_execution::testing::store_fixtures::admit_root_for_test(
        &store,
        &lease,
        &lash_core_execution::TurnId::from("wake-source-root"),
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
            session_id: SessionId::from(session_id.to_string()),
            ..lash_core_execution::RuntimeSessionState::new(
                lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
            )
        };
        completion_store
            .commit_runtime_state(finishing_root(
                lash_core_execution::RuntimeCommit::persisted_state_for_test(&state),
                &lease,
                "wake-source-root",
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
    timeout_wake.wake_id = "wake:source-lock-timeout".to_string();
    timeout_wake.sequence = 2;
    timeout_wake.event_invocation.subject =
        lash_core_execution::runtime::RuntimeSubject::ProcessEvent {
            process_id: timeout_wake.process_id.clone(),
            sequence: timeout_wake.sequence,
            event_type: timeout_wake.event_type.clone(),
        };
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
        .seal_drive_epoch_for_test(
            &SessionId::from(session_id),
            &second_owner,
            "wake-executor-2",
            60_000,
        )
        .await
        .expect("claim target for second sequence")
        .acquired()
        .expect("second-sequence target lease");
    let second_admission = lash_core_execution::testing::store_fixtures::admit_root_for_test(
        &store,
        &second_lease,
        &lash_core_execution::TurnId::from("wake-source-second-root"),
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
        .commit_runtime_state(finishing_root(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state),
            &second_lease,
            "wake-source-second-root",
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

lash_conformance::process_prune_session_store_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres process-owned session prune conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let factory = Arc::new(storage.store()) as Arc<dyn DeploymentStore>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let (promise_guard, effect_host) = promise_authority().await;
    (
        (_database_fixture, promise_guard),
        factory,
        registry,
        effect_host,
    )
});

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
            session_id: SessionId::from(SESSION_ID.to_string()),
            relation: lash_core_execution::SessionRelation::default(),
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
            )
            .into(),
            head: lash_core_execution::SessionCreationHead::CommittedByCreator,
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
        .seal_drive_epoch_for_test(
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
        session_id: SessionId::from(SESSION_ID.to_string()),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    let operation = lash_core_execution::OperationId::turn(SESSION_ID, TURN_ID, "final");
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

// `from_pool` and `connect` must both reject a reader floor above this build.
// The altered stamp lives in a scratch schema so no failed assertion can affect
// another conformance case.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_from_pool_enforces_schema_version_gate_when_configured() {
    let Some(url) = database_url() else {
        eprintln!("skipping Postgres from_pool gate test: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let scratch = IsolatedSchema::provision(&url).await;
    let pool = scratch.pool.clone();
    let current_version: i32 = sqlx::query_scalar(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&pool)
    .await
    .expect("read current schema version");
    assert_eq!(current_version, 1, "the 1.0 compatibility stamp changed");
    let payload_hash_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'lash_usage_facts'
           AND column_name = 'payload_hash'",
    )
    .fetch_one(&pool)
    .await
    .expect("payload_hash column exists");
    assert_eq!(payload_hash_nullable, "NO");
    let usage_identity_constraint: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
         FROM pg_constraint
         WHERE conrelid = 'lash_usage_facts'::regclass
           AND contype = 'u'",
    )
    .fetch_one(&pool)
    .await
    .expect("read usage fact identity uniqueness constraint");
    assert!(
        usage_identity_constraint.contains(
            "owner_kind, owner_id, effect_key, call_ordinal, provider_attempt, fact_kind"
        ),
        "a usage fact's identity is its owner, effect, call, attempt and kind (ADR 0125): \
         {usage_identity_constraint}"
    );
    // A newer catalog whose floor passed every version this build reads, in
    // its tier, must refuse adoption.
    let newer_version = i32::try_from(
        lash_core_execution::compat::descriptor(lash_core_execution::compat::ComponentId::POSTGRES)
            .expect("the build declares the PostgreSQL store")
            .reads
            .max()
            + 1,
    )
    .expect("the component version fits");
    sqlx::query(
        "UPDATE lash_schema_versions SET version = $1, min_reader = $1
         WHERE component = 'lash-postgres-store'",
    )
    .bind(newer_version)
    .execute(&pool)
    .await
    .expect("raise reader floor");

    let result = PostgresStorage::from_pool(pool.clone()).await;
    scratch.cleanup().await;
    assert!(matches!(
        result,
        Err(StoreError::Incompatible {
            refusal: CompatRefusal::ReaderFloorAbove {
                found,
                min_reader,
                ..
            }
        }) if i32::try_from(found) == Ok(newer_version)
            && i32::try_from(min_reader) == Ok(newer_version)
    ));
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

#[path = "conformance/admission_crash_cells.rs"]
mod admission_crash_cells;
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

// FIG-3607 contract 4 (FIG-4489): every logical turn a drive runs, a
// recovered follow-on's included, is owned by `Turn(logical root)`, on the
// Restate double over PostgreSQL stores.
mod driver_turn_ownership {
    use super::*;
    lash_conformance::driver_turn_ownership_tests!({
        let Some((lock, storage)) = storage().await else {
            return;
        };
        reset(storage.pool()).await;
        let ((attachments, double), stores, host, runner) = double_law_backend(&storage).await;
        (
            (lock, storage, attachments, double),
            "pg-driver-ownership",
            host,
            stores,
            runner,
        )
    });
}

mod root_control {
    use super::*;
    lash_conformance::drive_admission_tests!(@laws [] {
        let Some((lock, storage)) = storage().await else { return; };
        reset(storage.pool()).await;
        // Drive-admission turns run inside the engine's handlers: the double
        // lends each attempt the handler-scoped controller a Restate tier
        // runs it on, over this test's PostgreSQL stores.
        let ((attachments, double), stores, host, runner) =
            double_law_backend(&storage).await;
        ((lock, storage, attachments, double), "pg-root-control", host, stores, runner)
    }; [
    (a_terminal_root_never_reparks, "s7b-0"),
    (one_unfinished_root_per_session, "root-one-unfinished"),
    (admission_delivers_every_row_it_binds, "root-admission-delivers"),
    (a_root_admission_is_idempotent_across_new_rows_and_fences, "root-admission-idempotent"),
    (a_root_admission_survives_a_worker_crash_without_widening, "drive-admission-commit-crash"),
    (a_diverged_root_parks_once_holds_its_admitted_rows_blocks_admission_and_completes_after_restore, "s7b-15"),
    (an_exhausted_root_parks_engine_retry_exhausted_via_reconcile_idempotently_with_no_evidence, "s7b-13"),
    (a_parked_roots_fence_stays_current_until_a_verb, "s7b-14"),

    (sends_behind_a_parked_root_commit_but_are_not_admitted, "s7b-8"),
    (redrive_under_a_restored_build_completes_once_and_clears_the_park, "s7b-9"),
    (a_stale_redrive_is_fenced_by_a_later_cancel, "s7b-10"),
    (root_scope_close_runs_after_terminal_evidence_at_least_once_never_for_parked, "s7b-11"),
    (a_joined_inputs_turn_scope_closes_with_its_admitting_root, "drive-joined-scope-close"),
    (a_root_crashed_at_its_report_handover_still_closes_its_scope, "s7b-11b"),
    (cancel_fork_and_close_raise_the_drive_epoch_and_redrive_does_not, "s7b-12"),

    (cancel_of_a_parked_root_writes_cancelled_settles_its_input_and_drains_the_next, "s7b-1"),
    (no_row_stays_bound_after_a_roots_verb_close_or_lost_end, "root-verb-unbinds"),
    (a_refused_root_ends_once_and_its_next_input_admits_a_new_root, "refused-root-end"),
    (a_root_with_no_engine_run_ends_only_once_it_started, "lost-root-no-run"),
    (an_obsolete_executor_never_ends_its_successors_root, "obsolete-executor"),
    (inconsistent_divergence_still_parks_on_a_lower_revision, "inconsistent-lower-revision"),
    (inconsistent_divergence_still_parks_on_another_leaf, "inconsistent-other-leaf"),
    (inconsistent_divergence_still_parks_on_another_checkpoint, "inconsistent-other-checkpoint"),
    (fork_releases_the_old_owner_before_the_new_root_drives_in_original_order_on_a_fresh_journal, "s7b-2"),
    (verbs_are_park_id_cas, "s7b-3"),
    (redrive_under_the_same_build_reparks_the_same_park_with_attempts_plus_one, "s7b-4"),
    (cancel_or_fork_of_a_redriving_root_is_refused, "s7b-5"),
    (an_intent_survives_a_crash_at_every_gap_and_reconcile_completes_it, "s7b-6"),
    (engine_refusals_are_retained_and_listed, "s7b-7"),
    (a_root_parked_on_a_later_physical_turn_is_cleared_by_its_commit, "s7b-16"),
    (a_redrive_the_root_ran_past_is_never_applied_again, "s7b-17"),
    (a_stale_paused_listing_never_reparks_a_resumed_root, "s7b-18"),
    (a_parked_session_is_asked_to_drive_only_through_its_ingress_obligation, "s7b-19"),
    (a_send_racing_an_unsettled_redrive_is_refused_until_the_redrive_settles, "l2-1"),
    (a_lost_redrive_ack_is_settled_by_reconcile_and_the_queued_send_is_admitted, "l2-2"),
    (a_failing_child_cancel_never_wedges_its_roots_cancel_or_fork, "s8c-1"),
    (a_delivery_whose_claim_was_retaken_never_settles_its_intent, "s8c-2"),
    (an_intent_whose_engine_half_keeps_failing_stalls_at_its_ceiling_and_unwedges_its_session, "s8c-3"),
    (a_refused_follow_on_drive_keeps_the_intents_obligation_due, "s8c-4"),
    (an_idle_session_admits_its_turn_lane_in_enqueue_order_whatever_the_kind, "drive-idle-turn-lane-order"),
    (a_command_enqueued_after_an_input_roots_admission_waits_for_the_next_boundary, "drive-command-after-admission"),
    (a_turn_never_takes_an_item_past_an_earlier_unconsumed_item_of_the_other_kind, "drive-turn-lane-contiguous"),
    (a_command_roots_redrive_replays_its_recorded_outcome, "drive-command-root-redrive"),
    ]);

    lash_conformance::queued_input_roots_tests!({
        let Some((lock, storage)) = storage().await else {
            return;
        };
        reset(storage.pool()).await;
        let ((attachments, double), stores, host, runner) = double_law_backend(&storage).await;
        (
            (lock, storage, attachments, double),
            "pg-queued-input-roots",
            host,
            stores,
            runner,
        )
    });

    lash_conformance::root_answers_its_rows_tests!({
        let Some((lock, storage)) = storage().await else {
            return;
        };
        reset(storage.pool()).await;
        let ((attachments, double), stores, host, runner) = double_law_backend(&storage).await;
        (
            (lock, storage, attachments, double),
            "pg-root-rows",
            host,
            stores,
            runner,
        )
    });
}

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

mod vm_broker {
    use super::*;
    // FIG-4159: the worker-broker laws, each turn inside a handler of the
    // Restate double over this test's PostgreSQL stores; a lost worker fails
    // the attempt and the double replays the invocation into the re-drive.
    lash_conformance::vm_broker_tests!({
        let Some((lock, storage)) = storage().await else {
            return;
        };
        reset(storage.pool()).await;
        let ((attachments, double), _stores, _host, runner) = double_law_backend(&storage).await;
        (
            (lock, storage, attachments, double),
            "pg-vm-broker".to_string(),
            runner,
        )
    });
}

mod frame_open {
    use super::*;
    // FIG-4110: every frame open (a context-pressure frame, a pressure frame
    // followed by `continue_as`, an administrative compaction) killed at each
    // crash point and redriven opens once, chained in order, with one
    // summarizer call, over this test's PostgreSQL stores.
    lash_conformance::frame_open_redrive_tests!({
        let Some((lock, storage)) = storage().await else {
            return;
        };
        reset(storage.pool()).await;
        let ((attachments, double), stores, host, runner) = double_law_backend(&storage).await;
        (
            (lock, storage, attachments, double),
            "pg-frame-open",
            host,
            stores,
            runner,
        )
    });
}

mod bound_trigger_duplicate {
    use super::*;
    // FIG-4297: a duplicate of a bound trigger delivery's occurrence, emitted
    // by a fresh invocation after the bound process was pruned, returns that
    // process and starts nothing, and the original emission's replay still
    // answers it, over this test's PostgreSQL stores.
    lash_conformance::bound_trigger_duplicate_tests!({
        let Some((lock, storage)) = storage().await else {
            return;
        };
        reset(storage.pool()).await;
        let ((attachments, double), stores, host, runner) = double_law_backend(&storage).await;
        (
            (lock, storage, attachments, double),
            "pg-bound-trigger",
            host,
            stores,
            runner,
        )
    });
}

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
            sqlx::query("ALTER TABLE lash_attachment_referrer_edges ADD CONSTRAINT ck_attachment_referrer_edges_kind CHECK (referrer_kind IN ('session', 'upload', 'execution', 'start_input', 'process_record')) NOT VALID").execute(&mut *tx).await.map_err(|error| StoreError::Backend(error.to_string()))?;
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

mod worker_recovery {
    use super::*;
    lash_conformance::worker_recovery_tests!({
        let Some((lock, storage)) = storage().await else {
            return;
        };
        reset(storage.pool()).await;
        let (_held, stores, _host, _runner) = double_law_backend(&storage).await;
        let recovery = stores.worker_recovery();
        ((lock, storage, _held), recovery)
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_attachment_materialization_turn_witnesses() {
    let Some((_lock, storage)) = storage().await else {
        panic!("attachment turn witness requires PostgreSQL");
    };
    reset(storage.pool()).await;
    let (_guard, stores, host, runner) = double_law_backend(&storage).await;
    lash_conformance::attachment_materialization_turn_witnesses("postgres", host, stores, runner)
        .await;
}

lash_conformance::usage_ledger_store_tests!({
    let Some((lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let snapshot_pool = storage.pool().clone();
    let snapshot: lash_conformance::UsageLedgerSnapshot = Arc::new(move || {
        let pool = snapshot_pool.clone();
        Box::pin(async move {
            let tables: Vec<String> = sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname = current_schema() AND tablename LIKE 'lash_%' AND tablename NOT LIKE 'lash_usage_%' ORDER BY tablename")
                .fetch_all(&pool).await.unwrap();
            let mut snapshot = Vec::new();
            for table in tables {
                let mut rows: Vec<String> =
                    sqlx::query_scalar(&format!("SELECT to_jsonb(t)::text FROM \"{table}\" AS t"))
                        .fetch_all(&pool)
                        .await
                        .unwrap();
                rows.sort();
                snapshot.push((table, rows.join("\n")));
            }
            snapshot
        })
    });
    let store = Arc::new(storage.store());
    let fixture = lash_conformance::UsageLedgerStoreFixture {
        accounting: store.clone(),
        factory: store,
        snapshot,
    };
    ((lock, storage), fixture)
});
mod session_commands {
    use super::*;
    lash_conformance::session_command_replay_tests!({
        let (lock, storage) = storage().await.expect("command laws require PostgreSQL");
        reset(storage.pool()).await;
        let ((attachments, double), stores, host, runner) = double_law_backend(&storage).await;
        (
            (lock, storage, attachments, double),
            "pg-commands",
            host,
            stores,
            runner,
        )
    });
}
