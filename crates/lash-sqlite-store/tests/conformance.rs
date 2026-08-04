//! Runs the backend-agnostic `ProcessRegistry` conformance suite against the
//! Sqlite implementation. The same suite runs against the in-memory registry
//! in lash-core, so both backends are held to one contract.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lash_core::runtime::RuntimeScope;
use lash_core::testing::conformance::{
    ReopenableProcessRegistry, ReopenableRuntimePersistence, ReopenableTriggerStore,
};
use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    ProcessExecutionEnvStore, ProcessRegistry, Resolution, ResolveOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation,
    RuntimePersistence, SessionStoreFactory, TriggerStore,
};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteEffectReplayOptions, SqliteProcessRegistry,
    SqliteRuntimeEffectController, SqliteSessionStoreFactory, SqliteTriggerStore, Store,
};
use tempfile::TempDir;

#[path = "../../lash-core/tests/support/cold_process_turn_parent.rs"]
mod cold_process_turn_parent;

fn fresh_db_path(dirs: &Arc<Mutex<Vec<TempDir>>>, file_name: &str) -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(file_name);
    dirs.lock().expect("dirs lock").push(dir);
    path
}

fn durable_turn_scope(session_id: impl Into<String>, turn_id: impl Into<String>) -> ExecutionScope {
    let session_id = session_id.into();
    ExecutionScope::turn(&session_id, turn_id)
}

async fn open_ephemeral_effect_controller(
    scope: ExecutionScope,
) -> (TempDir, SqliteRuntimeEffectController) {
    let dir = tempfile::tempdir().expect("effect replay tempdir");
    let controller =
        SqliteRuntimeEffectController::open(&dir.path().join("effect-replay.db"), scope)
            .await
            .expect("file-backed effect controller");
    (dir, controller)
}

fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

fn open_registry(path: &Path) -> Arc<dyn ProcessRegistry> {
    let path = path.to_path_buf();
    let sessions = path.with_extension("sessions");
    Arc::new(sync_await(async move {
        SqliteProcessRegistry::open(&path, sessions)
            .await
            .expect("file registry")
    })) as Arc<dyn ProcessRegistry>
}

fn open_store(path: &Path) -> Arc<dyn RuntimePersistence> {
    let path = path.to_path_buf();
    Arc::new(sync_await(async move {
        Store::open(&path).await.expect("file store")
    })) as Arc<dyn RuntimePersistence>
}

fn artifact_store_handles(
    path: &Path,
) -> lash_lashlang_runtime::testing::conformance::ArtifactStoreHandles {
    let path = path.to_path_buf();
    let store = Arc::new(sync_await(async move {
        Store::open(&path).await.expect("file artifact store")
    }));
    lash_lashlang_runtime::testing::conformance::ArtifactStoreHandles {
        artifacts: Arc::clone(&store) as Arc<dyn lashlang::LashlangArtifactStore>,
        process_env: store as Arc<dyn ProcessExecutionEnvStore>,
    }
}

fn open_trigger_store(path: &Path) -> Arc<dyn TriggerStore> {
    let path = path.to_path_buf();
    Arc::new(sync_await(async move {
        SqliteTriggerStore::open(&path)
            .await
            .expect("file trigger store")
    })) as Arc<dyn TriggerStore>
}

#[tokio::test]
async fn sqlite_artifact_store_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_lashlang_runtime::testing::conformance::artifact_store_reopenable(|| {
        let path = fresh_db_path(&dirs, "artifacts.db");
        let reopen_path = path.clone();
        lash_lashlang_runtime::testing::conformance::ReopenableArtifactStore {
            open: artifact_store_handles(&path),
            reopen: Arc::new(move || artifact_store_handles(&reopen_path)),
        }
    })
    .await;
}

fn exec_envelope(replay_key: &str, code: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("effect-session", "effect-turn", 1, 0),
            replay_key,
            RuntimeEffectKind::ExecCode,
            replay_key,
        ),
        RuntimeEffectCommand::ExecCode {
            language: "code".to_string(),
            code: code.to_string(),
        },
    )
}

fn exec_outcome(marker: &str) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ExecCode {
        result: Box::new(Ok(lash_core::ExecResponse {
            observations: Vec::new(),
            observation_truncation: Vec::new(),
            tool_calls: Vec::new(),
            images: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 0,
            terminal_finish: Some(serde_json::json!(marker)),
        })),
    }
}

fn assert_exec_marker(outcome: RuntimeEffectOutcome, expected: &str) {
    let RuntimeEffectOutcome::ExecCode { result } = outcome else {
        panic!("expected exec-code outcome");
    };
    let response = result.expect("exec-code response");
    assert_eq!(response.terminal_finish, Some(serde_json::json!(expected)));
}

fn returning_executor(marker: &'static str) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |_| async move { Ok(exec_outcome(marker)) })
}

fn failing_executor() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async move {
        Err(RuntimeEffectControllerError::new(
            "test_local_executor_called",
            "replay must not invoke the local executor",
        ))
    })
}

fn current_epoch_ms_for_test() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Debug)]
struct ConformanceClock(std::sync::atomic::AtomicU64);

impl ConformanceClock {
    fn new(timestamp_ms: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(timestamp_ms))
    }

    fn advance(&self, duration_ms: u64) {
        self.0
            .fetch_add(duration_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl lash_core::Clock for ConformanceClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.timestamp_ms()),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(deadline.into()).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_registry_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::process_registry_reopenable(|| {
        let path = fresh_db_path(&dirs, "processes.db");
        ReopenableProcessRegistry {
            open: open_registry(&path),
            reopen: open_registry(&path),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_leased_completion_replay_repairs_projection() {
    let dir = tempfile::tempdir().expect("leased replay repair tempdir");
    let path = dir.path().join("processes.db");
    let registry = Arc::new(
        SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
            .await
            .expect("open leased replay repair registry"),
    );
    let corruption_path = path.clone();
    lash_core::testing::conformance::leased_completion_replay_repairs_projection(
        registry as Arc<dyn ProcessRegistry>,
        move |stale| async move {
            let conn = rusqlite::Connection::open(corruption_path)
                .expect("open projection corruption connection");
            let changed = conn
                .execute(
                    "UPDATE processes SET record_json = ?2 WHERE process_id = ?1",
                    rusqlite::params![
                        stale.id,
                        serde_json::to_string(&stale).expect("encode stale process projection")
                    ],
                )
                .expect("corrupt SQLite process projection");
            assert_eq!(changed, 1);
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_trigger_retention_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::process_trigger_retention(move || {
        let dirs = Arc::clone(&dirs);
        async move {
            let dir = tempfile::tempdir().expect("process-trigger retention tempdir");
            let registry = Arc::new(
                SqliteProcessRegistry::open(
                    &dir.path().join("processes.db"),
                    dir.path().join("sessions"),
                )
                .await
                .expect("process registry"),
            ) as Arc<dyn ProcessRegistry>;
            let triggers = Arc::new(
                SqliteTriggerStore::open(&dir.path().join("triggers.db"))
                    .await
                    .expect("trigger store"),
            ) as Arc<dyn TriggerStore>;
            dirs.lock().expect("retention dirs lock").push(dir);
            lash_core::testing::conformance::ProcessTriggerRetentionHandles { registry, triggers }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_store_contract_state_machine_properties() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::store_contract_state_machine("sqlite", move |seed| {
        let dirs = Arc::clone(&dirs);
        async move {
            let dir = tempfile::tempdir().expect("store-contract tempdir");
            let registry_path = dir.path().join(format!("processes-{seed}.db"));
            let runtime_path = dir.path().join(format!("runtime-{seed}.db"));
            let sessions = dir.path().join("sessions");
            let registry = Arc::new(
                SqliteProcessRegistry::open(&registry_path, sessions)
                    .await
                    .expect("open property process registry"),
            ) as Arc<dyn ProcessRegistry>;
            let runtime = Arc::new(
                Store::open(&runtime_path)
                    .await
                    .expect("open property runtime store"),
            ) as Arc<dyn RuntimePersistence>;
            dirs.lock().expect("property tempdirs lock").push(dir);
            lash_core::testing::conformance::StoreContractHandles { registry, runtime }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_runtime_persistence_state_machine_properties() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::runtime_persistence_state_machine("sqlite", move |seed| {
        let dirs = Arc::clone(&dirs);
        async move {
            let dir = tempfile::tempdir().expect("runtime-persistence property tempdir");
            let path = dir.path().join(format!("runtime-{seed}.db"));
            let runtime = Arc::new(
                Store::open(&path)
                    .await
                    .expect("open property runtime store"),
            ) as Arc<dyn RuntimePersistence>;
            dirs.lock().expect("property tempdirs lock").push(dir);
            runtime
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_session_graph_state_machine_properties() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::session_graph_state_machine("sqlite", move |_| {
        let dirs = Arc::clone(&dirs);
        async move {
            let dir = tempfile::tempdir().expect("session-graph property tempdir");
            let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()))
                as Arc<dyn SessionStoreFactory>;
            dirs.lock().expect("session-graph tempdirs lock").push(dir);
            factory
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_continuation_store_satisfies_conformance() {
    let storage = Arc::new(
        SqliteProcessRegistry::memory()
            .await
            .expect("open continuation store"),
    );
    let registry = Arc::clone(&storage) as Arc<dyn lash_core::ProcessRegistry>;
    let store = storage as Arc<dyn lash_core::ProcessContinuationStore>;
    lash_core::testing::conformance::process_continuation_store(registry, store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_wake_delivery_crash_matrix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let process_registry_path = dir.path().join("processes.db");
    let clock = Arc::new(
        lash_core::testing::conformance::WakeDeliveryConformanceClock::new(1_800_000_000_000),
    );
    let registry = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &process_registry_path,
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
            dir.path().join("sessions"),
        )
        .await
        .expect("open process registry")
        .with_wake_delivery_config(
            lash_core::WakeDeliveryConfig::new(10_000)
                .expect("valid test retention")
                .with_enqueuing_stale_after_ms(25)
                .expect("valid short stale-claim age"),
        ),
    ) as Arc<dyn ProcessRegistry>;
    let factory = Arc::new(
        SqliteSessionStoreFactory::new_with_process_registry(dir.path(), process_registry_path)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    ) as Arc<dyn SessionStoreFactory>;
    Box::pin(lash_core::testing::conformance::wake_delivery_crash_matrix(
        factory, registry, clock,
    ))
    .await;
}

#[tokio::test]
async fn sqlite_process_registry_rejects_pre_unit_external_owner_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-unit-external-owner-processes.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy process db");
    conn.pragma_update(None, "user_version", 12)
        .expect("stamp legacy process schema");
    drop(conn);

    let error = match SqliteProcessRegistry::open(&path, dir.path().join("sessions")).await {
        Ok(_) => panic!("pre-unit-external-owner process stores must be recreated"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash process registry schema"));
    assert!(message.contains("supports schema version 20"));
    assert!(message.contains("delete the process registry database and start fresh"));
}

#[tokio::test]
async fn sqlite_session_store_factory_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::session_store_factory(
        || {
            let dir = tempfile::tempdir().expect("tempdir");
            let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()))
                as Arc<dyn SessionStoreFactory>;
            dirs.lock().expect("dirs lock").push(dir);
            factory
        },
        || {
            Arc::new(sync_await(Store::memory()).expect("in-memory SQLite store"))
                as Arc<dyn RuntimePersistence>
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_fork_observer_intent_transient_failure_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    lash_core::testing::conformance::fork_observer_intent_transient_failure(Arc::new(
        SqliteSessionStoreFactory::new(dir.path()),
    ))
    .await;
}

#[tokio::test]
async fn sqlite_session_graph_append_branch_liveness_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    lash_core::testing::conformance::session_graph_append_branch_liveness(Arc::new(
        SqliteSessionStoreFactory::new(dir.path()),
    )
        as Arc<dyn SessionStoreFactory>)
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_attachment_owner_cold_replay_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(ConformanceClock::new(
        current_epoch_ms_for_test().saturating_sub(100_000),
    ));
    let process_path = dir.path().join("processes.db");
    let registry = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &process_path,
            clock.clone(),
            dir.path().join("sessions"),
        )
        .await
        .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;
    let factory = Arc::new(
        SqliteSessionStoreFactory::new_with_process_registry(
            dir.path().join("sessions"),
            &process_path,
        )
        .with_clock(clock.clone()),
    ) as Arc<dyn SessionStoreFactory>;
    let effect_path = dir.path().join("effects.db");
    let scope = durable_turn_scope("attachment-owner-cold-replay", "attachment-owner-turn");
    let first = Arc::new(
        SqliteRuntimeEffectController::open_with_clock(&effect_path, scope.clone(), clock.clone())
            .await
            .expect("first effect controller"),
    ) as Arc<dyn RuntimeEffectController>;
    let reopen_effect_controller = {
        let effect_path = effect_path.clone();
        let clock = clock.clone();
        Arc::new(move || {
            let effect_path = effect_path.clone();
            let scope = scope.clone();
            let clock = clock.clone();
            Box::pin(async move {
                Arc::new(
                    SqliteRuntimeEffectController::open_with_clock(&effect_path, scope, clock)
                        .await
                        .expect("cold replay effect controller"),
                ) as Arc<dyn RuntimeEffectController>
            })
                as std::pin::Pin<Box<dyn Future<Output = Arc<dyn RuntimeEffectController>> + Send>>
        })
    };
    let advance_clock = {
        let clock = clock.clone();
        Arc::new(move |duration_ms| clock.advance(duration_ms)) as Arc<dyn Fn(u64) + Send + Sync>
    };

    lash_core::testing::conformance::attachment_owner_cold_replay(
        lash_core::testing::conformance::AttachmentOwnerColdReplayBackend {
            session_store_factory: factory,
            process_registry: registry,
            attachment_store: Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
            first_effect_controller: Some(first),
            reopen_effect_controller,
            clock,
            advance_clock,
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_process_prune_deletes_owned_session_stores() {
    let dir = tempfile::tempdir().expect("tempdir");
    let process_path = dir.path().join("processes.db");
    let sessions = dir.path().join("sessions");
    let registry = Arc::new(
        SqliteProcessRegistry::open(&process_path, &sessions)
            .await
            .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;
    let factory = Arc::new(SqliteSessionStoreFactory::new_with_process_registry(
        &sessions,
        &process_path,
    )) as Arc<dyn SessionStoreFactory>;

    lash_core::testing::conformance::process_prune_deletes_owned_session_stores(factory, registry)
        .await;
}

#[tokio::test]
async fn sqlite_store_uses_injected_clock_for_expiry() {
    let clock = Arc::new(ConformanceClock::new(20_000));
    let store = Arc::new(
        Store::memory_with_clock(clock.clone())
            .await
            .expect("clock-driven sqlite store"),
    ) as Arc<dyn RuntimePersistence>;
    lash_core::testing::conformance::runtime_persistence_clock_expiry(store, |duration_ms| {
        clock.advance(duration_ms);
    })
    .await;
}

#[tokio::test]
async fn sqlite_trigger_store_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::trigger_store_reopenable(|| {
        let path = fresh_db_path(&dirs, "triggers.db");
        ReopenableTriggerStore {
            open: open_trigger_store(&path),
            reopen: open_trigger_store(&path),
        }
    })
    .await;
}

#[tokio::test]
async fn sqlite_trigger_store_rejects_pre_keyed_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-keyed-triggers.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy trigger db");
    conn.pragma_update(None, "user_version", 1)
        .expect("stamp legacy trigger schema");
    drop(conn);

    let error = match SqliteTriggerStore::open(&path).await {
        Ok(_) => panic!("pre-keyed trigger stores must be recreated"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash trigger store schema"));
    assert!(message.contains("supports schema version 3"));
    assert!(message.contains("delete the trigger store database and start fresh"));
}

#[tokio::test]
async fn sqlite_effect_controller_rejects_pre_retirement_journal_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-canonical-envelope-effects.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy effect db");
    conn.pragma_update(None, "user_version", 4)
        .expect("stamp legacy effect schema");
    drop(conn);

    let error =
        match SqliteRuntimeEffectController::open(&path, durable_turn_scope("session", "turn"))
            .await
        {
            Ok(_) => panic!("pre-retirement effect stores must be recreated"),
            Err(error) => error,
        };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash effect replay schema"));
    assert!(message.contains("supports schema version 7"));
    assert!(message.contains("delete the effect replay database and start fresh"));
}

#[tokio::test]
async fn sqlite_trigger_ingress_skips_malformed_matching_subscription() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("malformed-trigger.db");
    let source_type = "ui.button.pressed";
    let source_key =
        lash_core::facade_support::empty_trigger_source_key(source_type).expect("source key");
    let store = SqliteTriggerStore::open(&path)
        .await
        .expect("open trigger store");
    let register = |owner: &str, key: &str| lash_core::TriggerCommand::Register {
        owner_scope: lash_core::TriggerOwnerScope::session(owner),
        actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(owner)),
        draft: lash_core::TriggerSubscriptionDraft::for_process(
            key,
            lash_core::ProcessExecutionEnvRef::new(format!("process-env:{owner}")),
            source_type,
            source_key.clone(),
            lash_core::ProcessInput::Engine {
                kind: "test".to_string(),
                payload: serde_json::json!({ "owner": owner }),
            },
            lash_core::ProcessIdentity::new("test"),
        )
        .with_payload_schema(lash_core::LashSchema::any()),
    };
    let malformed = store
        .execute_command("register-malformed", register("malformed", "malformed-key"))
        .await
        .expect("execute malformed registration")
        .expect("register malformed row");
    let current = store
        .execute_command("register-current", register("current", "current-key"))
        .await
        .expect("execute current registration")
        .expect("register current row");
    let lash_core::TriggerCommandOutcome::Mutation { receipt: malformed } = malformed else {
        panic!("expected malformed registration receipt")
    };
    let lash_core::TriggerCommandOutcome::Mutation { receipt: current } = current else {
        panic!("expected current registration receipt")
    };
    drop(store);

    let conn = rusqlite::Connection::open(&path).expect("open raw trigger db");
    conn.execute(
        "UPDATE trigger_subscriptions SET record_json = ?2 WHERE subscription_id = ?1",
        rusqlite::params![malformed.subscription_id.as_str(), "{not valid json"],
    )
    .expect("poison trigger row");
    drop(conn);

    let reopened = SqliteTriggerStore::open(&path)
        .await
        .expect("reopen trigger store");
    let ingress = reopened
        .ingest_occurrence(lash_core::TriggerOccurrenceRequest::new(
            source_type,
            source_key,
            serde_json::json!({ "button": "Blue" }),
            "malformed-row-occurrence",
        ))
        .await
        .expect("one malformed row must not halt trigger ingress");
    assert_eq!(ingress.reservations.len(), 1);
    assert_eq!(
        ingress.reservations[0].subscription.subscription_id,
        current.subscription_id
    );
}

#[tokio::test]
async fn sqlite_store_satisfies_runtime_persistence_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::runtime_persistence_reopenable(|| {
        let path = fresh_db_path(&dirs, "session.db");
        ReopenableRuntimePersistence {
            open: open_store(&path),
            reopen: open_store(&path),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_runtime_persistence_recovery_laws() {
    let dir = tempfile::tempdir().expect("store-recovery tempdir");
    lash_core::testing::conformance::runtime_persistence_recovery_laws(|scenario| {
        open_store(&dir.path().join(format!("store-recovery-{scenario}.db")))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_real_turn_crash_matrix() {
    let dir = tempfile::tempdir().expect("real-turn crash matrix tempdir");
    Box::pin(lash_core::testing::conformance::turn_crash_matrix_level_1(
        |scenario| open_store(&dir.path().join(format!("turn-crash-matrix-{scenario}.db"))),
    ))
    .await;
}

#[tokio::test]
async fn sqlite_checkpoint_component_refs_survive_cold_reopens() {
    let dir = tempfile::tempdir().expect("checkpoint-component tempdir");
    let path = dir.path().join("checkpoint-components.db");
    lash_core::testing::conformance::checkpoint_component_refs_survive_cold_reopens(|| {
        open_store(&path)
    })
    .await;
}

#[tokio::test]
async fn sqlite_append_receipt_replays_after_ancestor_superseded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("append-receipt-ancestor.db");
    let store = Arc::new(Store::open(&path).await.expect("open store"));
    let mutation_path = path.clone();
    lash_core::testing::conformance::append_request_receipt_replays_after_ancestor_superseded(
        store as Arc<dyn RuntimePersistence>,
        move |leaf_node_id| async move {
            let conn = rusqlite::Connection::open(mutation_path).expect("open raw sqlite");
            conn.execute(
                "UPDATE session_head
                 SET leaf_node_id = ?1, head_revision = head_revision + 1
                 WHERE session_id = 'root'",
                rusqlite::params![leaf_node_id],
            )
            .expect("switch sqlite active branch");
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_append_receipt_restores_mixed_usage_envelope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(&dir.path().join("append-receipt-mixed-envelope.db"))
            .await
            .expect("open store"),
    );
    lash_core::testing::conformance::append_receipt_mixed_usage_envelope(store).await;
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn sqlite_cancelled_queued_append_publishes_usage_exactly_once() {
    use lash_sqlite_store::testing::{SqliteFaultInjector, SqliteFaultPoint};

    let dir = tempfile::tempdir().expect("tempdir");
    let injector = SqliteFaultInjector::default();
    let factory = SqliteSessionStoreFactory::new(dir.path()).with_fault_injector(injector.clone());
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "root".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::default(),
        })
        .await
        .expect("create cancellation store");
    lash_core::testing::conformance::append_usage_cancellation_publishes_exactly_once(
        store,
        move || {
            let pause = injector.pause_after(SqliteFaultPoint::BeforeCommit, 1);
            async move {
                pause.wait_until_reached().await;
                move || pause.release()
            }
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_old_format_append_receipt_returns_public_leaf() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("append-receipt-old-format.db");
    let store = Arc::new(Store::open(&path).await.expect("open store"));
    lash_core::testing::conformance::old_format_append_receipt_returns_public_leaf(
        store,
        move || async move {
            let conn = rusqlite::Connection::open(path).expect("open raw SQLite receipt fixture");
            let result_json: String = conn
                .query_row(
                    "SELECT result_json FROM runtime_turn_commits
                     WHERE turn_id LIKE '%old-format-append-receipt%'
                       AND turn_id NOT LIKE '%old-format-append-receipt-seed%'",
                    [],
                    |row| row.get(0),
                )
                .expect("read runtime receipt JSON");
            let mut result: serde_json::Value =
                serde_json::from_str(&result_json).expect("decode runtime receipt JSON");
            let fields = result.as_object_mut().expect("receipt result object");
            fields.remove("committed_leaf_node_id");
            fields.remove("receipt_replayed");
            conn.execute(
                "UPDATE runtime_turn_commits
                 SET result_json = ?1
                 WHERE turn_id LIKE '%old-format-append-receipt%'
                   AND turn_id NOT LIKE '%old-format-append-receipt-seed%'",
                rusqlite::params![serde_json::to_string(&result).expect("encode old receipt")],
            )
            .expect("install raw pre-upgrade receipt fixture");
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_store_schema_excludes_embedded_turn_replay_tables() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("schema.db");
    drop(Store::open(&path).await.expect("open store"));
    let conn = rusqlite::Connection::open(&path).expect("open raw sqlite");
    for removed in [
        concat!("runtime_", "turn_", "checkpoints"),
        concat!("runtime_", "effect_", "journal"),
    ] {
        let count = raw_count(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            removed,
        );
        assert_eq!(count, 0, "{removed} table must not exist");
    }
    let turn_commits = raw_count(
        &conn,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        "runtime_turn_commits",
    );
    assert_eq!(turn_commits, 1);
}

#[tokio::test]
async fn sqlite_runtime_turn_receipt_identity_columns_are_nullable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("receipt-schema.db");
    drop(Store::open(&path).await.expect("open store"));
    let conn = rusqlite::Connection::open(path).expect("open raw sqlite");
    let mut stmt = conn
        .prepare("PRAGMA table_info(runtime_turn_commits)")
        .expect("prepare receipt schema query");
    let columns = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(3)?))
        })
        .expect("query receipt schema")
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()
        .expect("collect receipt schema");
    for column in [
        "request_identity_hash",
        "requested_node_count",
        "requested_ancestor_node_id",
        "identity_encoding_version",
    ] {
        assert_eq!(columns.get(column), Some(&0), "{column} must allow NULL");
    }
}

fn raw_count(conn: &rusqlite::Connection, sql: &str, name: &str) -> i64 {
    conn.query_row(sql, rusqlite::params![name], |row| row.get::<_, i64>(0))
        .expect("query sqlite_master")
}

#[tokio::test]
async fn sqlite_effect_host_satisfies_scope_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("effect-host.db");
    lash_core::testing::conformance::effect_host(move || {
        let path = path.clone();
        Arc::new(sync_await(async move {
            SqliteEffectHost::open(&path).await.expect("effect host")
        })) as Arc<dyn EffectHost>
    })
    .await;
}

#[tokio::test]
async fn sqlite_effect_host_and_controller_reject_non_file_backed_path_spellings() {
    for path in [
        "",
        ":memory:",
        "file::memory:?cache=shared",
        "file:temporary",
    ] {
        let error = match SqliteEffectHost::open(Path::new(path)).await {
            Ok(_) => panic!("effect hosts must reject non-file-backed path {path:?}"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("requires a file-backed database path"),
            "unexpected error for {path:?}: {error}"
        );

        let error = match SqliteRuntimeEffectController::open(
            Path::new(path),
            durable_turn_scope("guard-session", "guard-turn"),
        )
        .await
        {
            Ok(_) => panic!("effect controllers must reject non-file-backed path {path:?}"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("requires a file-backed database path"),
            "unexpected controller error for {path:?}: {error}"
        );
    }
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn sqlite_completion_key_permission_tracks_backing_not_replay_ownership() {
    let memory =
        SqliteRuntimeEffectController::memory(durable_turn_scope("memory-session", "memory-turn"))
            .await
            .expect("testing-only memory controller");
    assert_eq!(
        memory.replay_ownership(),
        lash_core::EffectReplayOwnership::Controller
    );
    assert!(!memory.allows_process_lifetime_completion_keys());

    let (_file_dir, file) =
        open_ephemeral_effect_controller(durable_turn_scope("file-session", "file-turn")).await;
    assert_eq!(
        file.replay_ownership(),
        lash_core::EffectReplayOwnership::Controller
    );
    assert!(file.allows_process_lifetime_completion_keys());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_effect_host_satisfies_cold_instance_await_event_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cold-await-event.db");
    lash_core::testing::conformance::effect_host_await_events_cold_instance(|| {
        let path = path.clone();
        Arc::new(sync_await(async move {
            SqliteEffectHost::open(&path)
                .await
                .expect("cold SQLite effect host")
        })) as Arc<dyn EffectHost>
    })
    .await;
}

#[tokio::test]
async fn sqlite_await_event_key_mint_is_pure_and_store_secret_is_stable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pure-await-event-key.db");
    let scope = durable_turn_scope("pure-key-session", "pure-key-turn");
    let wait = AwaitEventWaitIdentity::tool_completion("pure-key-call");

    let (first, second) = tokio::join!(
        async {
            SqliteEffectHost::open(&path)
                .await
                .expect("first concurrent host")
                .await_event_key(&scope, wait.clone())
                .await
                .expect("first concurrent key")
        },
        async {
            SqliteEffectHost::open(&path)
                .await
                .expect("second concurrent host")
                .await_event_key(&scope, wait.clone())
                .await
                .expect("second concurrent key")
        },
    );
    assert_eq!(
        first, second,
        "concurrent openers must read one store secret"
    );

    let connection = rusqlite::Connection::open(&path).expect("open raw effect database");
    let wait_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM await_event_waits", [], |row| {
            row.get(0)
        })
        .expect("count await-event waits");
    assert_eq!(wait_count, 0, "key mint must not register a promise row");
    let secret_shape: (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), length(MAX(signing_secret)) FROM await_event_meta",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("inspect await-event signer");
    assert_eq!(secret_shape, (1, 32));
}

#[tokio::test]
async fn sqlite_effect_host_satisfies_cold_process_await_event_conformance() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::process::Command;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cold-process-await-event.db");
    for identity in ["tool_completion", "turn_cancel_gate"] {
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_sqlite-await-event-helper"))
            .arg(&path)
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

        let resolver = Arc::new(
            SqliteEffectHost::open(&path)
                .await
                .expect("cold-process resolver"),
        );
        let terminal = if identity == "turn_cancel_gate" {
            let address = lash_core::runtime::TurnAddress::new(
                format!("cold-process-{nonce}-session"),
                format!("cold-process-{nonce}-turn"),
            );
            let receipt = lash_core::runtime::TurnWorkDriver::new(
                Arc::clone(&resolver) as Arc<dyn EffectHost>
            )
            .request_cancel(lash_core::runtime::TurnCancelRequest::new(
                address,
                format!("cold-process-{nonce}-cancel"),
                None,
            ))
            .await
            .expect("request cancellation through a successor owner");
            assert!(matches!(
                receipt.outcome,
                lash_core::runtime::TurnCancelOutcome::Requested(_)
            ));
            resolver
                .peek_await_event(&key)
                .await
                .expect("peek successor cancellation")
                .expect("successor cancellation resolves the killed owner's gate")
        } else {
            let terminal = Resolution::Ok(serde_json::json!({
                "cold_process": true,
                "identity": identity,
                "nonce": nonce,
            }));
            assert_eq!(
                resolver
                    .resolve_await_event(&key, terminal.clone())
                    .await
                    .unwrap_or_else(|error| panic!(
                        "resolve killed-helper {identity} key: {error}"
                    )),
                ResolveOutcome::Accepted
            );
            terminal
        };
        drop(resolver);

        let observer = SqliteEffectHost::open(&path)
            .await
            .expect("cold-process observer");
        assert_eq!(
            observer
                .peek_await_event(&key)
                .await
                .unwrap_or_else(|error| panic!("peek killed-helper {identity} key: {error}")),
            Some(terminal.clone())
        );
        assert_eq!(
            observer
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None,)
                .await
                .unwrap_or_else(|error| panic!("observe killed-helper {identity} key: {error}")),
            terminal
        );
    }
}

#[tokio::test]
async fn sqlite_effect_replay_satisfies_cold_process_crash_conformance() {
    use tokio::process::Command;

    let dir = tempfile::tempdir().expect("cold-process effect replay tempdir");
    let database = dir.path().join("cold-process-effect-replay.db");
    let marker = dir.path().join("external-effect.log");
    let nonce = uuid::Uuid::new_v4().to_string();
    let run = |action: &'static str| {
        let database = database.clone();
        let marker = marker.clone();
        let nonce = nonce.clone();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                Command::new(env!("CARGO_BIN_EXE_sqlite-await-event-helper"))
                    .arg(database)
                    .arg(action)
                    .arg(nonce)
                    .arg(marker)
                    .output(),
            )
            .await
            .unwrap_or_else(|_| panic!("{action} helper timed out"))
            .unwrap_or_else(|error| panic!("spawn {action} helper: {error}"))
        }
    };

    let crashed = run("effect_crash").await;
    assert_eq!(crashed.status.code(), Some(86));
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read crashed effect marker")
            .lines()
            .count(),
        1,
        "the external effect ran before the owner crashed"
    );

    let completed = run("effect_complete").await;
    assert!(
        completed.status.success(),
        "successor helper failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read re-executed effect marker")
            .lines()
            .count(),
        2,
        "an unrecorded external effect is honestly re-executed"
    );

    let replayed = run("effect_replay").await;
    assert!(
        replayed.status.success(),
        "replay helper failed: {}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read replay effect marker")
            .lines()
            .count(),
        2,
        "a recorded outcome replays without another external effect"
    );
}

#[tokio::test]
async fn sqlite_real_turn_satisfies_cold_process_crash_matrix() {
    let dir = tempfile::tempdir().expect("SQLite cold-process real-turn tempdir");
    let database = dir.path().join("cold-process-real-turn.db");
    cold_process_turn_parent::assert_real_turn_kill_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command =
                tokio::process::Command::new(env!("CARGO_BIN_EXE_sqlite-await-event-helper"));
            command.arg(&database).arg(action).arg(nonce).arg(marker);
            command
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_effect_controller_satisfies_replay_conformance() {
    let (_controller_dir, controller) = open_ephemeral_effect_controller(durable_turn_scope(
        "effect-conformance-session",
        "effect-conformance-turn",
    ))
    .await;

    lash_core::testing::conformance::effect_controller_concurrent_replay_deterministic(
        &controller,
        || controller.start_replay(),
    )
    .await;

    let (_tool_controller_dir, tool_controller) =
        open_ephemeral_effect_controller(durable_turn_scope(
            "tool-attempt-conformance-session",
            "tool-attempt-conformance-turn",
        ))
        .await;
    lash_core::testing::conformance::effect_controller_tool_attempt_fanout_replay_deterministic(
        &tool_controller,
        || tool_controller.start_replay(),
    )
    .await;

    let (_durable_controller_dir, durable_controller) = open_ephemeral_effect_controller(
        durable_turn_scope("durable-step-session", "durable-step-turn"),
    )
    .await;
    lash_core::testing::conformance::effect_controller_journaled_effect_replay(
        &durable_controller,
        || durable_controller.start_replay(),
    )
    .await;
}

#[tokio::test]
async fn sqlite_effect_controller_replays_without_local_executor() {
    let (_controller_dir, controller) =
        open_ephemeral_effect_controller(durable_turn_scope("session", "turn")).await;
    let envelope = exec_envelope("exec-replay", "first");
    let first = controller
        .execute_effect(envelope.clone(), returning_executor("recorded"))
        .await
        .expect("first effect");
    assert_exec_marker(first, "recorded");

    controller.start_replay();
    let replayed = controller
        .execute_effect(envelope, failing_executor())
        .await
        .expect("replayed effect");
    assert_exec_marker(replayed, "recorded");
}

#[tokio::test]
async fn sqlite_effect_host_retires_session_journal_rows() {
    let dir = tempfile::tempdir().expect("effect replay tempdir");
    let path = dir.path().join("effect-replay.db");
    let host = SqliteEffectHost::open(&path)
        .await
        .expect("open SQLite effect host");

    lash_core::testing::conformance::effect_host_retires_session_journal(&host).await;
    lash_core::testing::conformance::effect_host_retires_process_journal(&host).await;

    let conn = rusqlite::Connection::open(path).expect("open effect journal for row count");
    let retained: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_effect_replay WHERE session_id = ?1",
            ["retired-journal-session"],
            |row| row.get(0),
        )
        .expect("count retained session journal rows");
    assert_eq!(retained, 0);
    let process_retained: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_effect_replay WHERE session_id IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count retained process journal rows");
    assert_eq!(process_retained, 0);
}

#[tokio::test]
async fn sqlite_effect_controller_reports_envelope_divergent_paths() {
    let (_controller_dir, controller) =
        open_ephemeral_effect_controller(durable_turn_scope("session", "turn")).await;
    lash_core::testing::conformance::effect_controller_replay_mismatch_diagnostics(
        &controller,
        "sqlite_effect_replay_hash_conflict",
    )
    .await;
}

#[tokio::test]
async fn sqlite_effect_controller_satisfies_lease_fencing_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    let path = fresh_db_path(&dirs, "effect-lease-fencing.db");
    let make_path = path.clone();
    let steal_path = path.clone();
    let expire_path = path.clone();
    lash_core::testing::conformance::effect_controller_lease_fencing(
        lash_core::testing::conformance::EffectLeaseFencingBackend {
            make_controller: Box::new(move |ttl| {
                let path = make_path.clone();
                Box::pin(async move {
                    let controller = SqliteRuntimeEffectController::open_with_options(
                        &path,
                        durable_turn_scope("session", "turn"),
                        SqliteEffectReplayOptions {
                            lease_timings: lash_core::facade_support::LeaseTimings::from_ttl(ttl)
                                .expect("conformance lease timings"),
                        },
                    )
                    .await
                    .expect("controller");
                    let for_replay = controller.clone();
                    lash_core::testing::conformance::LeaseFencingController {
                        controller: Arc::new(controller),
                        start_replay: Box::new(move || for_replay.start_replay()),
                    }
                })
            }),
            steal_lease: Box::new(move |replay_key| {
                let path = steal_path.clone();
                Box::pin(async move {
                    let stolen_until = current_epoch_ms_for_test().saturating_add(10_000);
                    let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                    let changed = conn
                        .execute(
                            "UPDATE runtime_effect_replay
                             SET lease_owner_id = 'stolen-owner',
                                 lease_token = 'stolen-token',
                                 lease_expires_at_ms = ?1
                             WHERE replay_key = ?2",
                            rusqlite::params![stolen_until as i64, replay_key],
                        )
                        .expect("steal lease row");
                    assert_eq!(changed, 1);
                })
            }),
            expire_lease: Box::new(move |replay_key| {
                let path = expire_path.clone();
                Box::pin(async move {
                    let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                    let changed = conn
                        .execute(
                            "UPDATE runtime_effect_replay
                             SET lease_expires_at_ms = 0
                             WHERE replay_key = ?1",
                            rusqlite::params![replay_key],
                        )
                        .expect("expire lease row");
                    assert_eq!(changed, 1);
                })
            }),
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_sleep_replay_returns_after_recorded_due_time() {
    let (_controller_dir, controller) =
        open_ephemeral_effect_controller(durable_turn_scope("session", "turn")).await;
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("session", "turn", 1, 0),
            "sleep",
            RuntimeEffectKind::Sleep,
            "sleep-key",
        ),
        RuntimeEffectCommand::Sleep { duration_ms: 120 },
    );

    let started = std::time::Instant::now();
    let first = controller
        .execute_effect(envelope.clone(), RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("first sleep");
    assert!(matches!(first, RuntimeEffectOutcome::Sleep));
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(100),
        "first sleep must wait until the recorded due_at"
    );

    controller.start_replay();
    let replayed = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        controller.execute_effect(envelope, failing_executor()),
    )
    .await
    .expect("replay must not sleep the full original duration")
    .expect("sleep replay");
    assert!(matches!(replayed, RuntimeEffectOutcome::Sleep));
}
