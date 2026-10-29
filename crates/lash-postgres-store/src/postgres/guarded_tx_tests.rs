//! The PostgreSQL writer fence against a move of `F` (ADR 0115 §2.2–2.4),
//! each on its own isolated database.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use lash_core_store::compat::VersionRange;
use lash_core_store::session_identity::SessionRelation;
use lash_core_store::store::FleetFormat;
use lash_core_store::store::SessionCatalogStore as _;
use lash_core_store::store::SessionCommitStore as _;
use lash_core_store::store::SessionMeta;
use lash_core_store::store::StoreError;
use lash_core_store::store::WriterPin;
use lash_execution_contracts::process::ProcessRegistrar as _;
use lash_execution_contracts::process::ProcessRetention as _;
use lash_kernel_value::identity::SessionId;

use super::WriterFence;
use crate::PostgresStorage;
use crate::testing::{AfterFence, HeldFinalize, IsolatedDatabase, finalize_fleet_epoch};

/// The epoch the synthetic next release finalizes to.
const NEXT: u32 = 2;

/// An epoch past every one this build writes under: a newer release
/// finalized it.
const PAST_WRITABLE: u32 = FleetFormat::writable().max() + 1;

async fn isolated() -> Option<IsolatedDatabase> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping writer-fence proof: database URL is not set");
        return None;
    };
    Some(IsolatedDatabase::create(&database_url).await)
}

/// Stand `storage` in for a build whose writable range is `writable`,
/// opened under `opened`.
fn standing(
    mut storage: PostgresStorage,
    writable: VersionRange,
    opened: FleetFormat,
) -> PostgresStorage {
    storage.fence = WriterFence::guarded(
        writable,
        opened,
        storage.fence.prelude().clone(),
        crate::host::RetryPolicies::default().store,
    );
    storage
}

fn root_meta(session_id: &SessionId) -> SessionMeta {
    SessionMeta {
        owning_process_id: None,
        session_id: session_id.clone(),
        relation: SessionRelation::Root,
        pending_observer_intents: Vec::new(),
    }
}

async fn recorded_epoch(storage: &PostgresStorage) -> i32 {
    sqlx::query_scalar("SELECT format_version FROM lash_fleet_format WHERE singleton")
        .fetch_one(storage.pool())
        .await
        .expect("read the recorded epoch")
}

async fn meta_rows(storage: &PostgresStorage, session_id: &SessionId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM lash_session_meta WHERE session_id = $1")
        .bind(session_id.as_str())
        .fetch_one(storage.pool())
        .await
        .expect("count session meta rows")
}

/// Waits until some backend of this database waits on a lock.
async fn until_a_lock_waiter(storage: &PostgresStorage) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(storage.pool())
            .await
            .expect("read lock waiters");
            if waiting > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("finalize waits on the writer's share lock");
}

/// A writer that passed its fence holds `F` shared: finalize waits for it,
/// and the writer commits under the old epoch before `F` moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_fence_orders_a_writer_before_finalize() {
    let Some(database) = isolated().await else {
        return;
    };
    let seam = AfterFence::new();
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store")
        .with_after_fence_for_testing(seam.clone());
    let session_id = SessionId::from("fence-orders-writer");

    let mut pause = seam.pause_next();
    let writer = tokio::spawn({
        let store = storage.store();
        let meta = root_meta(&session_id);
        async move {
            store.admit_session(&lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(meta)).await
        }
    });
    assert_eq!(
        pause.reached().await,
        1,
        "the writer's fence read the old epoch"
    );

    let finalize = tokio::spawn({
        let pool = storage.pool().clone();
        async move { finalize_fleet_epoch(&pool, NEXT).await }
    });
    until_a_lock_waiter(&storage).await;
    assert!(
        !finalize.is_finished(),
        "finalize must wait behind a writer holding the fence row"
    );
    assert_eq!(recorded_epoch(&storage).await, 1);

    pause.release();
    writer
        .await
        .expect("join the writer")
        .expect("the paused writer commits under the old epoch");
    finalize
        .await
        .expect("join finalize")
        .expect("finalize commits once the writer has");
    assert_eq!(meta_rows(&storage, &session_id).await, 1);
    assert_eq!(recorded_epoch(&storage).await, i32::try_from(NEXT).unwrap());
    assert_eq!(seam.passed(), vec![1]);
}

/// A writer that begins after finalize moved `F` past its build's range is
/// refused `WriterFenced` and writes nothing: through a pre-encoded session
/// write, and through a retried single-statement write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_fence_refuses_a_writer_after_finalize_with_zero_writes() {
    let Some(database) = isolated().await else {
        return;
    };
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let seeded = storage.fleet_format().version();
    finalize_fleet_epoch(storage.pool(), PAST_WRITABLE)
        .await
        .expect("finalize a release past this build");

    let session_id = SessionId::from("fence-refuses-writer");
    let error = storage
        .store()
        .admit_session(
            &lash_core_execution::testing::store_fixtures::session_request_from_meta_for_test(
                root_meta(&session_id),
            ),
        )
        .await
        .expect_err("a writer after finalize is fenced");
    assert!(
        matches!(
            error,
            StoreError::WriterFenced { recorded: PAST_WRITABLE, writable }
                if writable == FleetFormat::writable()
        ),
        "expected WriterFenced at epoch {PAST_WRITABLE}, got {error:?}"
    );
    assert_eq!(meta_rows(&storage, &session_id).await, 0);

    let error = storage
        .process_registry()
        .register_process(lash_execution_contracts::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .expect_err("a process-registry write after the move of F is fenced");
    assert!(
        error.to_string().contains("writer fenced"),
        "expected WriterFenced, got {error:?}"
    );
    let processes: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_processes")
        .fetch_one(storage.pool())
        .await
        .expect("count processes");
    assert_eq!(processes, 0, "a fenced writer wrote nothing");
    assert_eq!(
        storage.fleet_format().version(),
        seeded,
        "a refused epoch is not the one this build writes under"
    );
}

/// A commit encoded under the last observed `F` whose fence reads a moved,
/// still writable `F` rolls back, is encoded again under the new epoch, and
/// commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_fence_encodes_again_when_f_moves() {
    let Some(database) = isolated().await else {
        return;
    };
    // Epoch 1 pins the receipt at a version no build writes, so a receipt
    // encoded under it is told apart from one encoded under epoch 2.
    const PINNED: u32 = 7;
    let opened = FleetFormat::from_version(1).with_writer_pins(&[WriterPin {
        constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
        generation: 1,
        version: PINNED,
    }]);
    let storage = standing(
        crate::testing::connect(database.url())
            .await
            .expect("open the isolated store"),
        VersionRange::between(1, NEXT),
        opened,
    );
    let store = storage.store();
    let session_id = SessionId::from("fence-encodes-again");
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session_id),
        )
        .await
        .expect("admit the session under epoch 1");
    assert_eq!(store.fence.fleet(), opened);

    finalize_fleet_epoch(storage.pool(), NEXT)
        .await
        .expect("finalize the next release");
    let state = lash_core_store::session_state::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_store::session_state::RuntimeSessionState::fixture(
            lash_core_store::session_policy::SessionPolicy::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
        )
    };
    let receipt = store
        .commit_runtime_state(
            lash_core_store::store::RuntimeCommit::persisted_state_for_test(&state),
        )
        .await
        .expect("the commit re-encodes under the moved epoch and lands");
    assert_eq!(
        receipt.schema_version,
        lash_core_store::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION
    );

    let stored: String = sqlx::query_scalar(
        "SELECT result_json FROM lash_runtime_turn_commits WHERE session_id = $1",
    )
    .bind(session_id.as_str())
    .fetch_one(storage.pool())
    .await
    .expect("read the stored receipt");
    let stored: serde_json::Value = serde_json::from_str(&stored).expect("decode the receipt");
    assert_eq!(
        stored["schema_version"],
        serde_json::json!(lash_core_store::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
        "the stored receipt was encoded under epoch {NEXT}, not epoch 1's pin {PINNED}"
    );
    assert_eq!(storage.fleet_format().version(), NEXT);
}

/// A fence that meets contention behind a finalize retries from a fresh
/// `BEGIN`, and the retry reads the epoch finalize committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_fence_retries_contended_with_a_fresh_read() {
    let Some(database) = isolated().await else {
        return;
    };
    let seam = AfterFence::new();
    let storage = standing(
        crate::testing::connect_with(database.url(), &{
            let mut config = crate::PostgresHostConfig::default();
            config.guards.ordinary.lock =
                crate::host::ServerTimeout::Limit(Duration::from_millis(100));
            config
        })
        .await
        .expect("open the isolated store"),
        VersionRange::between(1, NEXT),
        FleetFormat::from_version(1),
    )
    .with_after_fence_for_testing(seam.clone());

    let held = HeldFinalize::begin(storage.pool(), NEXT)
        .await
        .expect("finalize holds the fence row");
    let writer = tokio::spawn({
        let registry = storage.process_registry();
        async move {
            registry
                .release_consumer_hold(
                    &lash_vocabulary::identity::ProcessId::fixture("fence-retry"),
                    "fence-retry",
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        while seam.contended() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the writer's fence times out behind finalize");
    held.commit().await.expect("finalize commits");

    writer
        .await
        .expect("join the writer")
        .expect("the retried writer commits under the new epoch");
    assert!(seam.contended() >= 1);
    assert_eq!(
        seam.passed(),
        vec![NEXT],
        "the retry's fence read the epoch finalize committed"
    );
    assert_eq!(storage.fleet_format().version(), NEXT);
}

/// FIG-6208: cancelling a pooled BEGIN while its fence waits must not return
/// an untracked server transaction to the next snapshot reader.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_fenced_begin_leaves_the_pool_ready_for_a_snapshot() {
    let Some(database) = isolated().await else {
        return;
    };
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .test_before_acquire(false)
        .connect(database.url())
        .await
        .expect("one pooled connection");
    let held = HeldFinalize::begin(storage.pool(), storage.fleet_format().version())
        .await
        .expect("hold the fence exclusive");
    let writer = tokio::spawn({
        let pool = pool.clone();
        let fence = storage.fence.clone();
        let prelude = fence.prelude().clone();
        async move { super::begin_durable(&pool, &fence, &prelude).await }
    });
    until_a_lock_waiter(&storage).await;
    writer.abort();
    assert!(
        writer
            .await
            .err()
            .expect("the waiting writer was cancelled")
            .is_cancelled()
    );
    drop(held);
    let connection = pool
        .acquire()
        .await
        .expect("reacquire the cancelled BEGIN's connection");
    let mut snapshot = sqlx::Transaction::begin(connection, None)
        .await
        .expect("start a fresh snapshot");
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *snapshot)
        .await
        .expect("the previous BEGIN left no queried transaction behind");
    snapshot.rollback().await.expect("close the snapshot");
    pool.close().await;
}

/// Holds one BEGIN's ReadyForQuery after the server entered its transaction,
/// before SQLx can acknowledge it and increment its transaction depth.
struct BeginReplyGate {
    port: u16,
    armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reached: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
    relay: tokio::task::JoinHandle<()>,
}

impl BeginReplyGate {
    async fn new(database_url: &str) -> Self {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let database: sqlx::postgres::PgConnectOptions =
            database_url.parse().expect("database URL");
        let address = (database.get_host().to_owned(), database.get_port());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind BEGIN reply gate");
        let port = listener.local_addr().expect("gate address").port();
        let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let relay = tokio::spawn({
            let (armed, reached, release) = (armed.clone(), reached.clone(), release.clone());
            async move {
                let (client, _) = listener.accept().await.expect("accept store connection");
                let server = tokio::net::TcpStream::connect(address)
                    .await
                    .expect("connect BEGIN reply gate");
                let (mut client_read, mut client_write) = client.into_split();
                let (mut server_read, mut server_write) = server.into_split();
                tokio::select! {
                    _ = tokio::io::copy(&mut client_read, &mut server_write) => {}
                    () = async {
                        loop {
                            let mut header = [0; 5];
                            if server_read.read_exact(&mut header).await.is_err() {
                                return;
                            }
                            let length = u32::from_be_bytes(header[1..].try_into().expect("message length"));
                            assert!((4..=1_048_576).contains(&length), "bounded test response");
                            let mut payload = vec![0; length as usize - 4];
                            server_read.read_exact(&mut payload).await.expect("server response");
                            if header[0] == b'Z' && payload == b"T"
                                && armed.swap(false, std::sync::atomic::Ordering::SeqCst)
                            {
                                reached.notify_one();
                                release.notified().await;
                            }
                            client_write.write_all(&header).await.expect("forward response header");
                            client_write.write_all(&payload).await.expect("forward response payload");
                        }
                    } => {}
                }
            }
        });
        Self {
            port,
            armed,
            reached,
            release,
            relay,
        }
    }
}

impl Drop for BeginReplyGate {
    fn drop(&mut self) {
        self.relay.abort();
    }
}

/// FIG-7551: cancelling a history BEGIN before its reply cannot return an
/// untracked transaction to the catalog lookup preceding an observe snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_history_begin_does_not_poison_the_next_observer_snapshot() {
    use lash_core_store::store::{SessionHistoryStore as _, WindowSelector};
    let database = IsolatedDatabase::create(&crate::testing::required_database_url()).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("store");
    let session = SessionId::from("cancelled-history-begin");
    let mut store = storage.store();
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session),
        )
        .await
        .expect("create the observed session");
    let gate = BeginReplyGate::new(database.url()).await;
    let options = database
        .url()
        .parse::<sqlx::postgres::PgConnectOptions>()
        .expect("database URL")
        .host("127.0.0.1")
        .port(gate.port)
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .test_before_acquire(false)
        .connect_with(options)
        .await
        .expect("one connection");
    store.pool = pool.clone();
    gate.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let reader = tokio::spawn({
        let (store, session) = (store.clone(), session.clone());
        async move {
            store
                .load_session_window(&session, WindowSelector::Current)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(30), gate.reached.notified())
        .await
        .expect("the server entered BEGIN before cancellation");
    reader.abort();
    assert!(
        reader
            .await
            .expect_err("cancel history read")
            .is_cancelled()
    );
    gate.release.notify_one();

    // The host's durable_session_head first looks up the session, then reads
    // its window. A leaked BEGIN lets that catalog read establish a snapshot
    // before read_tx tries to set its isolation level.
    assert!(matches!(
        store
            .lookup_session(&session)
            .await
            .expect("catalog lookup"),
        lash_core_store::store::SessionLookup::Live(_)
    ));
    let snapshot = store
        .load_session_window(&session, WindowSelector::Current)
        .await
        .expect("cancelled history BEGIN must not poison the next observe snapshot")
        .expect("the observed session retains its head");
    assert_eq!(snapshot.session_id, session);
    assert_eq!(snapshot.head_revision, 0);
    pool.close().await;
}

/// FIG-7552: PostgreSQL's invalid-transaction-state code survives a poisoned
/// snapshot read through the host fault and its serialized representation.
#[tokio::test]
async fn poisoned_history_read_preserves_sqlstate_in_host_fault() {
    use lash_core_store::store::{SessionHistoryStore as _, StoreFault, WindowSelector};
    let database = IsolatedDatabase::create(&crate::testing::required_database_url()).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("store");
    let mut store = storage.store();
    let session = SessionId::from("sqlstate-poisoned-history");
    store
        .admit_session(
            &lash_core_execution::testing::store_fixtures::root_session_request(&session),
        )
        .await
        .expect("create session");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .test_before_acquire(false)
        .connect(database.url())
        .await
        .expect("one connection");
    store.pool = pool.clone();
    {
        let mut connection = pool.acquire().await.expect("connection to poison");
        // Reproduce the untracked transaction FIG-7551 left behind, including
        // the catalog query that prevents changing its isolation level.
        sqlx::query("BEGIN")
            .execute(&mut *connection)
            .await
            .expect("untracked BEGIN");
        sqlx::query("SELECT 1")
            .execute(&mut *connection)
            .await
            .expect("establish snapshot");
    }
    let error = store
        .load_session_window(&session, WindowSelector::Current)
        .await
        .expect_err("cannot change isolation after a query");
    let fault = StoreFault::of_store_error(&error).expect("host store fault");
    for through_engine in [false, true] {
        let durable = lash_durable::DurableError::from(
            lash_execution_contracts::module_artifacts::ArtifactStoreError::StoreFault(
                fault.clone(),
            ),
        );
        let forwarded = if through_engine {
            lash_execution_contracts::plugin_error::PluginError::from(
                lash_execution_contracts::process::linking::EnginePortError::from(durable),
            )
        } else {
            lash_execution_contracts::plugin_error::registry_error(durable)
        };
        let lash_execution_contracts::plugin_error::PluginError::StoreUnavailable {
            fault: forwarded,
        } = forwarded
        else {
            panic!("database fault lost its host classification: {forwarded:?}");
        };
        assert_eq!(forwarded.storage_code(), fault.storage_code());
    }
    let encoded = serde_json::to_value(&fault).expect("serialize host fault");
    // PostgreSQL Appendix A defines 25001 as active_sql_transaction, class 25.
    assert_eq!(
        encoded["code"],
        serde_json::json!({
            "backend": "postgres", "sqlstate": "25001", "class": "25"
        }),
        "{error:?}"
    );
    assert_eq!(
        serde_json::from_value::<StoreFault>(encoded).expect("decode fault"),
        fault
    );
    pool.close().await;
}
