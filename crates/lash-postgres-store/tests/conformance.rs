use std::future::Future;
use std::sync::Arc;

use lash_core::testing::conformance::{
    ReopenableProcessRegistry, ReopenableRuntimePersistence, ReopenableTriggerStore,
};
use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, DurabilityTier, EffectHost,
    ExecutionScope, ProcessExecutionEnvStore, ProcessRegistry, Resolution, ResolveOutcome,
    RuntimePersistence, SessionStoreFactory, TriggerStore,
};
use lash_postgres_store::{
    PostgresEffectReplayOptions, PostgresRuntimeEffectController, PostgresStorage,
};

mod support;

use support::{SharedDatabaseLock, database_url};

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

async fn storage() -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let url = database_url()?;
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect postgres");
    Some((database_lock, storage))
}

async fn reset(storage: &PostgresStorage) {
    let pool = storage.pool();
    // Derive the truncate set from the live catalog rather than hand-maintaining
    // a table list: a new `lash_*` table can no longer silently bleed state
    // between conformance cases. `lash_schema_versions` is excluded — it holds
    // the component schema version gate, not per-case fixture rows.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("list lash_* conformance tables");
    assert!(
        !tables.is_empty(),
        "expected the lash_* schema tables to exist before reset"
    );
    let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
    sqlx::query(&truncate)
        .execute(pool)
        .await
        .expect("reset postgres conformance tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(pool)
    .await
    .expect("reset postgres process change clock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_graph_node_primary_key_is_global_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_runtime_persistence_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::runtime_persistence_reopenable(|| {
        let storage = Arc::clone(&storage);
        sync_await(async move {
            reset(&storage).await;
            let open = Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>;
            let reopen = Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>;
            ReopenableRuntimePersistence { open, reopen }
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_artifact_store_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres artifact-store conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    lash_lashlang_runtime::testing::conformance::artifact_store_reopenable(|| {
        let storage = Arc::clone(&storage);
        sync_await(async move {
            reset(&storage).await;
            let handles = || lash_lashlang_runtime::testing::conformance::ArtifactStoreHandles {
                artifacts: Arc::new(storage.lashlang_artifact_store())
                    as Arc<dyn lashlang::LashlangArtifactStore>,
                process_env: Arc::new(storage.process_env_store())
                    as Arc<dyn ProcessExecutionEnvStore>,
            };
            lash_lashlang_runtime::testing::conformance::ReopenableArtifactStore {
                open: handles(),
                reopen: handles(),
            }
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_session_store_factory_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres session-store-factory conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::session_store_factory(
        || {
            let storage = Arc::clone(&storage);
            sync_await(async move {
                reset(&storage).await;
                Arc::new(storage.session_store_factory()) as Arc<dyn SessionStoreFactory>
            })
        },
        DurabilityTier::Durable,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_attachment_owner_cold_replay_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres attachment-owner conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let storage = Arc::new(storage);
    let scope = ExecutionScope::turn("attachment-owner-cold-replay", "attachment-owner-turn");
    let first = Arc::new(storage.runtime_effect_controller(scope.clone()))
        as Arc<dyn lash_core::RuntimeEffectController>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let reopen_effect_controller = {
        let storage = Arc::clone(&storage);
        Arc::new(move || {
            let controller = Arc::new(storage.runtime_effect_controller(scope.clone()))
                as Arc<dyn lash_core::RuntimeEffectController>;
            Box::pin(async move { controller })
                as std::pin::Pin<
                    Box<dyn Future<Output = Arc<dyn lash_core::RuntimeEffectController>> + Send>,
                >
        })
    };
    let clock = Arc::new(
        lash_core::testing::conformance::AttachmentOwnerConformanceClock::new(
            lash_core::Clock::timestamp_ms(&lash_core::SystemClock).saturating_sub(100_000),
        ),
    );
    let factory = Arc::new(
        storage
            .session_store_factory_with_shared_process_registry()
            .with_clock(clock.clone()),
    ) as Arc<dyn SessionStoreFactory>;
    let advance_clock = {
        let clock = Arc::clone(&clock);
        Arc::new(move |duration_ms| clock.advance(duration_ms)) as Arc<dyn Fn(u64) + Send + Sync>
    };

    lash_core::testing::conformance::attachment_owner_cold_replay(
        lash_core::testing::conformance::AttachmentOwnerColdReplayBackend {
            session_store_factory: factory,
            process_registry: registry,
            attachment_store: Arc::new(lash_core::InMemoryAttachmentStore::new()),
            first_effect_controller: Some(first),
            reopen_effect_controller,
            clock,
            advance_clock,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_deletes_owned_session_stores_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres process-owned session prune conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let factory = Arc::new(storage.session_store_factory_with_shared_process_registry())
        as Arc<dyn SessionStoreFactory>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;

    lash_core::testing::conformance::process_prune_deletes_owned_session_stores(factory, registry)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_turn_commit_stamps_use_injected_store_clock_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres injected commit clock regression: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    const SESSION_ID: &str = "postgres-injected-commit-clock";
    const TURN_ID: &str = "postgres-injected-clock-turn";
    const NOW_MS: u64 = 1_234_567;
    let clock =
        Arc::new(lash_core::testing::conformance::AttachmentOwnerConformanceClock::new(NOW_MS));
    let factory = storage
        .session_store_factory_with_shared_process_registry()
        .with_clock(clock);
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: SESSION_ID.to_string(),
            relation: lash_core::SessionRelation::default(),
            policy: lash_core::SessionPolicy::default(),
        })
        .await
        .expect("create clocked Postgres session store");
    store
        .record_intent(lash_core::AttachmentIntent {
            attachment_id: lash_core::AttachmentId::new("postgres-clock-attachment"),
            session_id: SESSION_ID.to_string(),
            canonical_uri: "lash-attachment://postgres-clock-attachment".to_string(),
            intent_at_epoch_ms: NOW_MS.saturating_sub(1),
            owner_kind: Some(lash_core::AttachmentOwnerKind::Turn),
            owner_id: Some(TURN_ID.to_string()),
        })
        .expect("record turn-owned intent");
    let owner = lash_core::LeaseOwnerIdentity::opaque("clock-test", "clock-test-incarnation");
    let lease = store
        .try_claim_session_execution_lease(SESSION_ID, &owner, 60_000)
        .await
        .expect("claim clock test lease")
        .acquired()
        .expect("clock test lease acquired");
    let state = lash_core::RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..Default::default()
    };
    let mut commit = lash_core::RuntimeCommit::persisted_state(&state, &[]);
    let hash = commit.turn_commit_hash().expect("turn commit hash");
    let operation = lash_core::OperationId::turn(SESSION_ID, TURN_ID, "final");
    let operation_key = operation.storage_key().expect("canonical operation key");
    commit = commit
        .with_turn_commit(lash_core::RuntimeTurnCommitStamp::new(
            SESSION_ID, operation, hash,
        ))
        .with_session_execution_lease(lease.fence())
        .releasing_session_execution_lease(lease.completion());
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit with injected clock");

    let manifest_stamp: i64 = sqlx::query_scalar(
        "SELECT committed_at_ms FROM lash_attachment_manifest
         WHERE session_id = $1 AND owner_id = $2",
    )
    .bind(SESSION_ID)
    .bind(TURN_ID)
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

// Blocker 1: `from_pool` must enforce the same component schema-version gate as
// `connect`/`connect_with`. Writing the pre-unit-external-owner version (17) into
// `lash_schema_versions` and then constructing over the pool must fail loudly with
// the mismatch error, so a pre-cutover database can never be adopted post-bump.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_from_pool_enforces_schema_version_gate_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres from_pool gate test: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let pool = storage.pool().clone();
    // Force the recorded component version to a stale value.
    sqlx::query(
        "INSERT INTO lash_schema_versions (component, version) VALUES ('lash-postgres-store', 17)
         ON CONFLICT (component) DO UPDATE SET version = EXCLUDED.version",
    )
    .execute(&pool)
    .await
    .expect("write stale schema version");

    let result = PostgresStorage::from_pool(pool.clone()).await;

    // Restore the correct version BEFORE asserting so a failed assert never leaves
    // the shared database wedged for other cases.
    sqlx::query(
        "UPDATE lash_schema_versions SET version = 18 WHERE component = 'lash-postgres-store'",
    )
    .execute(&pool)
    .await
    .expect("restore schema version");

    let message = match result {
        Ok(_) => panic!("from_pool must reject a version-17 database"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains("version 17") && message.contains("expected 18"),
        "expected a schema-version mismatch error, got: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_host_satisfies_cold_instance_await_event_conformance_when_configured() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-instance AwaitEvent conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    lash_core::testing::conformance::effect_host_await_events_cold_instance(|| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("cold PostgreSQL effect host")
        });
        Arc::new(storage.effect_host()) as Arc<dyn EffectHost>
    })
    .await;
    drop(database_lock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_await_event_key_mint_is_pure_and_signatures_match_sqlite_when_seeded() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres await-event signing test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let scope = ExecutionScope::turn("pure-key-session", "pure-key-turn");
    let wait = AwaitEventWaitIdentity::tool_completion("pure-key-call");

    let (first, second) = tokio::join!(
        async {
            PostgresStorage::connect(&database_url)
                .await
                .expect("first concurrent storage")
                .effect_host()
                .await_event_key(&scope, wait.clone())
                .await
                .expect("first concurrent key")
        },
        async {
            PostgresStorage::connect(&database_url)
                .await
                .expect("second concurrent storage")
                .effect_host()
                .await_event_key(&scope, wait.clone())
                .await
                .expect("second concurrent key")
        },
    );
    assert_eq!(
        first, second,
        "concurrent openers must read one store secret"
    );
    let wait_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lash_await_event_waits")
        .fetch_one(storage.pool())
        .await
        .expect("count await-event waits");
    assert_eq!(wait_count, 0, "key mint must not register a promise row");
    let secret: Vec<u8> = sqlx::query_scalar(
        "SELECT signing_secret FROM lash_await_event_meta WHERE singleton = TRUE",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read PostgreSQL await-event signer");
    assert_eq!(secret.len(), 32);

    let directory = tempfile::tempdir().expect("SQLite parity tempdir");
    let sqlite_path = directory.path().join("signature-parity.db");
    drop(
        lash_sqlite_store::SqliteEffectHost::open(&sqlite_path)
            .await
            .expect("initialize SQLite parity store"),
    );
    let connection =
        rusqlite::Connection::open(&sqlite_path).expect("open raw SQLite parity store");
    connection
        .execute(
            "UPDATE await_event_meta SET signing_secret = ?1 WHERE singleton = 1",
            rusqlite::params![secret],
        )
        .expect("seed SQLite with PostgreSQL signing secret");
    drop(connection);
    let sqlite_key = lash_sqlite_store::SqliteEffectHost::open(&sqlite_path)
        .await
        .expect("reopen seeded SQLite parity store")
        .await_event_key(&scope, wait)
        .await
        .expect("SQLite parity key");
    assert_eq!(
        first, sqlite_key,
        "PostgreSQL and SQLite must emit byte-identical keys for identical secret and identity"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_host_satisfies_cold_process_await_event_conformance_when_configured() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::process::Command;

    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-process AwaitEvent conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    for identity in ["tool_completion", "turn_cancel_gate"] {
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_postgres-await-event-helper"))
            .arg(identity)
            .arg(&nonce)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn cold-process helper for {identity}: {error}"));
        let stdout = child.stdout.take().expect("helper stdout pipe");
        let mut lines = BufReader::new(stdout).lines();
        let encoded_key =
            tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("helper did not mint {identity} key"))
                .expect("read helper key")
                .unwrap_or_else(|| panic!("helper exited before printing {identity} key"));
        let key: AwaitEventKey = serde_json::from_str(&encoded_key)
            .unwrap_or_else(|error| panic!("decode helper {identity} key: {error}"));

        child
            .kill()
            .await
            .unwrap_or_else(|error| panic!("kill parked {identity} helper: {error}"));
        let status = child
            .wait()
            .await
            .unwrap_or_else(|error| panic!("reap parked {identity} helper: {error}"));
        assert!(
            !status.success(),
            "killed {identity} helper exited successfully"
        );

        let terminal = Resolution::Ok(serde_json::json!({
            "cold_process": true,
            "identity": identity,
            "nonce": nonce,
        }));
        let resolver =
            PostgresStorage::connect(&database_url().expect("configured Postgres database URL"))
                .await
                .expect("cold-process resolver")
                .effect_host();
        assert_eq!(
            resolver
                .resolve_await_event(&key, terminal.clone())
                .await
                .unwrap_or_else(|error| panic!("resolve killed-helper {identity} key: {error}")),
            ResolveOutcome::Accepted
        );
        drop(resolver);

        let observer =
            PostgresStorage::connect(&database_url().expect("configured Postgres database URL"))
                .await
                .expect("cold-process observer")
                .effect_host();
        assert_eq!(
            observer
                .peek_await_event(&key)
                .await
                .unwrap_or_else(|error| panic!("peek killed-helper {identity} key: {error}")),
            Some(terminal.clone())
        );
        assert_eq!(
            observer
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
                .await
                .unwrap_or_else(|error| panic!("observe killed-helper {identity} key: {error}")),
            terminal
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_runtime_effect_controller_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres runtime-effect conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;

    let controller = storage.runtime_effect_controller(ExecutionScope::runtime_operation(
        "postgres-effect-controller-conformance",
    ));
    lash_core::testing::conformance::effect_controller_journaled_effect_replay(&controller, || {
        controller.start_replay()
    })
    .await;

    let controller = storage.runtime_effect_controller(ExecutionScope::runtime_operation(
        "postgres-effect-controller-mismatch-conformance",
    ));
    lash_core::testing::conformance::effect_controller_replay_mismatch_diagnostics(
        &controller,
        "postgres_effect_replay_hash_conflict",
    )
    .await;

    let controller = storage.runtime_effect_controller(ExecutionScope::runtime_operation(
        "postgres-effect-controller-concurrent-conformance",
    ));
    lash_core::testing::conformance::effect_controller_concurrent_replay_deterministic(
        &controller,
        || controller.start_replay(),
    )
    .await;

    let controller = storage.runtime_effect_controller(ExecutionScope::runtime_operation(
        "postgres-effect-controller-tool-conformance",
    ));
    lash_core::testing::conformance::effect_controller_tool_attempt_fanout_replay_deterministic(
        &controller,
        || controller.start_replay(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_controller_satisfies_lease_fencing_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres effect lease-fencing conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;

    let make_storage = storage.clone();
    let steal_pool = storage.pool().clone();
    let expire_pool = storage.pool().clone();
    lash_core::testing::conformance::effect_controller_lease_fencing(
        lash_core::testing::conformance::EffectLeaseFencingBackend {
            make_controller: Box::new(move |ttl| {
                let storage = make_storage.clone();
                Box::pin(async move {
                    let controller = PostgresRuntimeEffectController::with_options(
                        &storage,
                        ExecutionScope::turn("session", "turn"),
                        PostgresEffectReplayOptions {
                            lease_timings: lash_core::LeaseTimings::from_ttl(ttl)
                                .expect("conformance lease timings"),
                        },
                    );
                    let for_replay = controller.clone();
                    lash_core::testing::conformance::LeaseFencingController {
                        controller: Arc::new(controller),
                        start_replay: Box::new(move || for_replay.start_replay()),
                    }
                })
            }),
            steal_lease: Box::new(move |replay_key| {
                let pool = steal_pool.clone();
                Box::pin(async move {
                    let stolen_until = epoch_ms_for_test().saturating_add(10_000) as i64;
                    let changed = sqlx::query(
                        "UPDATE lash_runtime_effect_replay
                         SET lease_owner_id = 'stolen-owner',
                             lease_token = 'stolen-token',
                             lease_expires_at_ms = $1
                         WHERE replay_key = $2",
                    )
                    .bind(stolen_until)
                    .bind(&replay_key)
                    .execute(&pool)
                    .await
                    .expect("steal lease row")
                    .rows_affected();
                    assert_eq!(changed, 1);
                })
            }),
            expire_lease: Box::new(move |replay_key| {
                let pool = expire_pool.clone();
                Box::pin(async move {
                    let changed = sqlx::query(
                        "UPDATE lash_runtime_effect_replay
                         SET lease_expires_at_ms = 0
                         WHERE replay_key = $1",
                    )
                    .bind(&replay_key)
                    .execute(&pool)
                    .await
                    .expect("expire lease row")
                    .rows_affected();
                    assert_eq!(changed, 1);
                })
            }),
        },
    )
    .await;
}

fn epoch_ms_for_test() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_registry_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::process_registry_reopenable(|| {
        let storage = Arc::clone(&storage);
        sync_await(async move {
            reset(&storage).await;
            let open = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
            let reopen = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
            ReopenableProcessRegistry { open, reopen }
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_trigger_store_satisfies_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres trigger conformance: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    lash_core::testing::conformance::trigger_store_reopenable(
        || {
            let storage = Arc::clone(&storage);
            sync_await(async move {
                reset(&storage).await;
                let open = Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>;
                let reopen = Arc::new(storage.trigger_store()) as Arc<dyn TriggerStore>;
                ReopenableTriggerStore { open, reopen }
            })
        },
        DurabilityTier::Durable,
    )
    .await;
}
