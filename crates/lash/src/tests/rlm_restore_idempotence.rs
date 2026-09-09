//! Same-frame `restore_session` on the real RLM plugin is idempotent
//! (FIG-2521).
//!
//! Every runtime path that restores the protocol session on the frame the
//! plugin already holds — resident reload after a follow-on failure, reopen-seed
//! receipt replay, and append rollback — must rebuild execution state and
//! projected bindings from the restore view instead of rejecting the seed the
//! plugin already bound. Each path is witnessed on the memory, SQLite and
//! PostgreSQL backends with the shipped RLM plugin, not a test protocol.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::facade_support::{
    EmbeddedRuntimeHost, InMemorySessionStoreFactory, LashRuntime, NativeRuntimeEffectController,
    PersistentRuntimeServices, PluginHost, PluginSession, RuntimeHostConfig,
    SingleProviderResolver, TurnOutcome,
};
use lash_core::plugin::{
    PluginFactory, PromptHookContext, RecordedSessionConfig, SessionStateService,
};
use lash_core::store::{RuntimeCommitReceipt, RuntimePersistenceDecorator};
use lash_core::{
    AppendSessionNodesRequest, CommitBudget, ExecutionScope, LlmOutputPart, LlmResponse, ModelSpec,
    PersistedSessionConfig, ProtocolTurnOptions, QueuedWorkBatchingConfig, RuntimeCommit,
    RuntimePersistence, RuntimeSessionState, ScopedEffectController, SessionAppendNode,
    SessionPolicy, SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, StoreError,
    TurnBudget, TurnInput,
};
use lash_protocol_rlm::{
    InstructionBound, MemoryBound, RlmProtocolPluginConfig, RlmProtocolPluginFactory, RlmSeed,
    WallClockBound, rlm_seed_initial_nodes,
};
use lash_sansio::sync::MutexExt;

/// One commit-level fault, consumed by the next `commit_runtime_state`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CommitFault {
    #[default]
    None,
    /// The commit is refused before it reaches the backend.
    FailNext,
    /// The commit lands, its acknowledgement is lost, and the retry replays
    /// the durable receipt.
    ReplayNext,
}

struct FaultStore {
    inner: Arc<dyn RuntimePersistence>,
    fault: Mutex<CommitFault>,
}

impl FaultStore {
    fn arm(&self, fault: CommitFault) {
        *self.fault.lock_recover() = fault;
    }
}

#[async_trait::async_trait]
impl RuntimePersistenceDecorator for FaultStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        let fault = std::mem::take(&mut *self.fault.lock_recover());
        match fault {
            CommitFault::None => self.inner.commit_runtime_state(commit).await,
            CommitFault::FailNext => Err(StoreError::Backend(
                "injected commit failure (FIG-2521)".to_string(),
            )),
            CommitFault::ReplayNext => {
                self.inner.commit_runtime_state(commit.clone()).await?;
                self.inner.commit_runtime_state(commit).await
            }
        }
    }
}

struct NoSessions;

#[async_trait::async_trait]
impl SessionStateService for NoSessions {}

fn policy() -> SessionPolicy {
    SessionPolicy {
        provider_id: "fig2521-rlm".to_string(),
        model: ModelSpec::builder("fig2521-model")
            .context_window_tokens(100_000)
            .build()
            .expect("model spec"),
        ..SessionPolicy::new(TurnBudget::Unbounded)
    }
}

fn seed_nodes(label: &str) -> Vec<SessionAppendNode> {
    let mut seed = RlmSeed::from_seed_value(&serde_json::json!({
        "payload": (0..64).collect::<Vec<_>>(),
        "baton": label,
    }))
    .expect("seed value");
    seed.projected.push(
        format!("projected_{label}"),
        lash_rlm_types::RlmProjectedSeedEntry::Materialized(serde_json::json!({ "value": label })),
    );
    rlm_seed_initial_nodes(seed)
}

fn plugin_host() -> PluginHost {
    PluginHost::new(vec![Arc::new(
        RlmProtocolPluginFactory::new(
            RlmProtocolPluginConfig::builder()
                .instruction_limit(InstructionBound::instructions(1_000_000))
                .wall_clock(WallClockBound::secs(30))
                .memory_limit(MemoryBound::mebibytes(64))
                .build(),
            Arc::new(crate::persistence::InMemoryLashlangArtifactStore::new()),
        )
        .with_process_lifecycle(false),
    ) as Arc<dyn PluginFactory>])
}

/// Scripted provider: one response per call, in order; the last response
/// repeats. Every request is recorded so a witness can inspect the prompt the
/// model actually saw.
#[derive(Default)]
struct Script {
    responses: Vec<String>,
    requests: Mutex<Vec<String>>,
    calls: AtomicUsize,
    arm_before_call: Mutex<Option<(usize, CommitFault)>>,
}

fn provider(
    script: Arc<Script>,
    store: Arc<FaultStore>,
) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("fig2521-rlm")
        .complete(move |request| {
            let script = Arc::clone(&script);
            let store = Arc::clone(&store);
            async move {
                let call = script.calls.fetch_add(1, Ordering::SeqCst);
                script.requests.lock_recover().push(format!("{request:?}"));
                if let Some((armed_call, fault)) = *script.arm_before_call.lock_recover()
                    && armed_call == call
                {
                    store.arm(fault);
                }
                let text = script
                    .responses
                    .get(call)
                    .or_else(|| script.responses.last())
                    .cloned()
                    .unwrap_or_default();
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle()
}

async fn open(
    store: Arc<FaultStore>,
    script: Arc<Script>,
    state: RuntimeSessionState,
) -> (LashRuntime, Arc<PluginSession>) {
    let host = plugin_host();
    let plugins = if let Some(snapshot) = state.plugin_state() {
        host.rematerialize_session(
            &state.session_id,
            snapshot,
            RecordedSessionConfig::new(state.protocol_turn_options.clone()),
        )
        .expect("rematerialize plugins")
    } else {
        host.build_session(&state.session_id)
            .expect("build plugins")
    };
    let mut config = RuntimeHostConfig::in_memory(
        CommitBudget::bounded(8 * 1024 * 1024, 1024),
        QueuedWorkBatchingConfig::new(1),
    );
    config.providers.provider_resolver = Arc::new(SingleProviderResolver::new(provider(
        script,
        Arc::clone(&store),
    )));
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        policy(),
        EmbeddedRuntimeHost::new(config),
        PersistentRuntimeServices::new(plugins.clone(), store as Arc<dyn RuntimePersistence>),
        state,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    // The facade fires protocol materialization on every root open (and
    // resume): the RLM plugin pins its channel into the recorded options.
    runtime
        .configure_protocol_on_materialize(&lash_core::PluginOptions::empty(), true)
        .expect("materialize protocol");
    (runtime, plugins)
}

/// The RLM plugin's prompt contribution for its projected bindings, rendered
/// as text so a witness can check which binding names reach the next prompt.
async fn projected_prompt(runtime: &LashRuntime, plugins: &PluginSession) -> String {
    let contributions = plugins
        .collect_prompt_contributions(PromptHookContext {
            session_id: runtime.read_view().session_id().to_string(),
            sessions: Arc::new(NoSessions),
            state: runtime.read_view(),
            protocol_turn_options: ProtocolTurnOptions::default(),
            turn_context: Default::default(),
        })
        .await
        .expect("prompt contributions");
    format!("{contributions:?}")
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

struct Backend {
    label: &'static str,
    factory: Arc<dyn SessionStoreFactory>,
    _tempdir: Option<tempfile::TempDir>,
}

impl Backend {
    fn memory() -> Self {
        Self {
            label: "memory",
            factory: Arc::new(InMemorySessionStoreFactory::new()),
            _tempdir: None,
        }
    }

    fn sqlite() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self {
            label: "sqlite",
            factory: Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                dir.path(),
            )),
            _tempdir: Some(dir),
        }
    }

    async fn postgres() -> Option<Self> {
        let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") {
                    panic!("LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1");
                }
                eprintln!(
                    "skipping PostgreSQL FIG-2521 witness: LASH_POSTGRES_DATABASE_URL is not set"
                );
                return None;
            }
        };
        let storage = lash_postgres_store::PostgresStorage::connect(&database_url)
            .await
            .expect("connect PostgreSQL");
        Some(Self {
            label: "postgres",
            factory: Arc::new(storage.session_store_factory()),
            _tempdir: None,
        })
    }

    /// Creates a fresh session store holding one durable projected seed
    /// (`projected_original`), reopens it cold so the live plugin holds that
    /// seed on the session's only frame, and returns the reopened runtime.
    async fn seeded_session(&self, scenario: &str, script: Arc<Script>) -> SeededSession {
        let session_id = format!(
            "fig2521-{scenario}-{}-{}",
            self.label,
            uuid::Uuid::new_v4().simple()
        );
        let request = SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: SessionRelation::Root,
            policy: policy(),
        };
        let base = self
            .factory
            .create_store(&request)
            .await
            .expect("create store");
        let store = Arc::new(FaultStore {
            inner: base.clone(),
            fault: Mutex::new(CommitFault::None),
        });
        let initial = RuntimeSessionState {
            session_id: session_id.clone(),
            protocol_turn_options: ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
                dialect: Some(lash_rlm_types::RlmDialect::Lashlang),
                ..Default::default()
            })
            .expect("rlm options"),
            ..RuntimeSessionState::new(policy())
        };
        let (mut runtime, plugins) = open(Arc::clone(&store), Arc::clone(&script), initial).await;
        runtime
            .append_session_nodes(AppendSessionNodesRequest {
                operation_id: "fig2521-seed".to_string(),
                requires_ancestor_node_id: None,
                nodes: seed_nodes("original"),
            })
            .await
            .expect("append seed");
        let snapshot = runtime
            .snapshot_execution_state()
            .await
            .expect("snapshot")
            .expect("execution state");
        assert!(
            !snapshot.components.is_empty(),
            "the witness must exercise real execution leaves"
        );
        runtime
            .restore_execution_state(&snapshot)
            .await
            .expect("restore snapshot");
        runtime.park().await.expect("park");
        drop(plugins);

        let durable = lash_core::store::load_persisted_session_state(base.as_ref())
            .await
            .expect("load")
            .expect("persisted state");
        let (mut runtime, plugins) = open(Arc::clone(&store), script, durable).await;
        let prompt = projected_prompt(&runtime, &plugins).await;
        assert_eq!(
            count(&prompt, "projected_original"),
            1,
            "{}: the reopened plugin must hold the durable seed once: {prompt}",
            self.label
        );
        let live_snapshot = runtime
            .snapshot_execution_state()
            .await
            .expect("snapshot")
            .expect("execution state");
        assert_eq!(
            live_snapshot, snapshot,
            "{}: reopen must restore the execution state",
            self.label
        );
        SeededSession {
            runtime,
            plugins,
            store,
            snapshot,
            prompt,
        }
    }
}

struct SeededSession {
    runtime: LashRuntime,
    plugins: Arc<PluginSession>,
    store: Arc<FaultStore>,
    snapshot: lash_core::plugin::HydratedExecutionState,
    prompt: String,
}

fn continue_as_response() -> String {
    // `projected_original` is a projected binding on the parent frame; the
    // seed-preserving projection policy carries it into the new frame as the
    // projected entry `carried`.
    "<lashlang>\nawait control.continue_as({task: \"next\", seed: {baton: \"switched\", carried: projected_original}})?\n</lashlang>".to_string()
}

fn turn_scope(runtime: &LashRuntime, turn_id: &str) -> ScopedEffectController<'static> {
    ScopedEffectController::shared(
        Arc::new(NativeRuntimeEffectController::default()),
        ExecutionScope::turn(runtime.read_view().session_id(), turn_id),
    )
    .expect("scope")
}

/// (a) A follow-on turn fails after an agent-frame switch; the next turn must
/// reload the invalidated resident state on the frame the plugin already
/// holds and re-bind that frame's projected seed instead of rejecting it.
async fn follow_on_failure_then_resident_reload(backend: Backend) {
    let script = Arc::new(Script {
        responses: vec![
            continue_as_response(),
            "second frame answer".to_string(),
            "after reload".to_string(),
        ],
        ..Script::default()
    });
    // The follow-on turn's commit fails: its provider call arms the fault.
    *script.arm_before_call.lock_recover() = Some((1, CommitFault::FailNext));
    let SeededSession {
        mut runtime,
        plugins,
        store,
        ..
    } = Box::pin(backend.seeded_session("follow-on", Arc::clone(&script))).await;
    let old_frame = runtime.export_persistence_state().current_frame_node_id;

    let run = runtime
        .run_turn_assembled(
            TurnInput::text("switch"),
            tokio_util::sync::CancellationToken::new(),
            turn_scope(&runtime, "fig2521-switch"),
        )
        .await
        .expect("a follow-on failure is reported on the committed switch turn");
    assert!(
        matches!(run.outcome, TurnOutcome::AgentFrameSwitch { .. }),
        "{}: {:?}",
        backend.label,
        run.outcome
    );
    assert!(
        !run.errors.is_empty(),
        "{}: the follow-on commit failure must be recorded on the switch turn",
        backend.label
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 2, "{}", backend.label);
    let new_frame = runtime.export_persistence_state().current_frame_node_id;
    assert_ne!(new_frame, old_frame, "{}", backend.label);
    assert_eq!(*store.fault.lock_recover(), CommitFault::None);

    let switched_prompt = projected_prompt(&runtime, &plugins).await;
    assert_eq!(
        count(&switched_prompt, "carried"),
        1,
        "{}: {switched_prompt}",
        backend.label
    );

    let run = runtime
        .run_turn_assembled(
            TurnInput::text("continue"),
            tokio_util::sync::CancellationToken::new(),
            turn_scope(&runtime, "fig2521-after-reload"),
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}: resident reload on the same frame must succeed: {error:?}",
                backend.label
            )
        });
    assert!(
        matches!(run.outcome, TurnOutcome::Finished(_)),
        "{}: {:?}",
        backend.label,
        run.outcome
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 3, "{}", backend.label);
    let reloaded_prompt = projected_prompt(&runtime, &plugins).await;
    assert_eq!(
        reloaded_prompt, switched_prompt,
        "{}: the reloaded plugin must hold exactly the switched frame's bindings",
        backend.label
    );
    let last_request = script.requests.lock_recover().last().cloned().unwrap();
    assert_eq!(
        count(&last_request, "`carried`"),
        1,
        "{}: the prompt after reload must bind the frame seed once: {last_request}",
        backend.label
    );
    assert_eq!(
        count(&last_request, "projected_original"),
        0,
        "{}: the old frame's seed must not leak into the switched frame",
        backend.label
    );
}

/// (b) The reopen-seed guard write replays its durable receipt; the runtime
/// discards its local seed and reloads the durable head on the frame the
/// plugin already holds.
async fn reopen_seed_receipt_replay(backend: Backend) {
    let script = Arc::new(Script {
        responses: vec!["after replay".to_string()],
        ..Script::default()
    });
    let SeededSession {
        mut runtime,
        plugins,
        store,
        snapshot,
        prompt,
    } = Box::pin(backend.seeded_session("reopen-seed", Arc::clone(&script))).await;

    let mut persisted = PersistedSessionConfig::from(&policy());
    persisted.model = ModelSpec::builder("fig2521-previous-model")
        .context_window_tokens(100_000)
        .build()
        .expect("model spec");
    store.arm(CommitFault::ReplayNext);
    lash_core::facade_support::settle_reopen_seeded_config(&mut runtime, &persisted)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}: reopen-seed receipt replay must reload the same frame: {error:?}",
                backend.label
            )
        });
    assert_eq!(*store.fault.lock_recover(), CommitFault::None);

    let replayed_prompt = projected_prompt(&runtime, &plugins).await;
    assert_eq!(replayed_prompt, prompt, "{}", backend.label);
    let replayed_snapshot = runtime
        .snapshot_execution_state()
        .await
        .expect("snapshot")
        .expect("execution state");
    assert_eq!(replayed_snapshot, snapshot, "{}", backend.label);

    let run = runtime
        .run_turn_assembled(
            TurnInput::text("go"),
            tokio_util::sync::CancellationToken::new(),
            turn_scope(&runtime, "fig2521-after-replay"),
        )
        .await
        .expect("turn after replay");
    assert!(
        matches!(run.outcome, TurnOutcome::Finished(_)),
        "{:?}",
        run.outcome
    );
    let last_request = script.requests.lock_recover().last().cloned().unwrap();
    assert_eq!(
        count(&last_request, "`projected_original`"),
        1,
        "{last_request}"
    );
}

/// (c) A faulted append rolls the protocol session back on the same frame;
/// the never-persisted `projected_discarded` binding must not survive into the
/// next prompt.
async fn faulted_append_rollback(backend: Backend) {
    let script = Arc::new(Script {
        responses: vec!["after rollback".to_string()],
        ..Script::default()
    });
    let SeededSession {
        mut runtime,
        plugins,
        store,
        snapshot,
        prompt,
    } = Box::pin(backend.seeded_session("append-rollback", Arc::clone(&script))).await;

    store.arm(CommitFault::FailNext);
    let error = runtime
        .append_session_nodes(AppendSessionNodesRequest {
            operation_id: "fig2521-discarded".to_string(),
            requires_ancestor_node_id: None,
            nodes: seed_nodes("discarded"),
        })
        .await
        .expect_err("the faulted append must fail");
    let message = error.to_string();
    assert!(
        message.contains("injected commit failure")
            && !message.contains("failed to restore protocol session"),
        "{}: the append must surface the store failure alone, never a rollback failure: {message}",
        backend.label
    );
    assert_eq!(*store.fault.lock_recover(), CommitFault::None);

    let rolled_back_prompt = projected_prompt(&runtime, &plugins).await;
    assert_eq!(
        rolled_back_prompt, prompt,
        "{}: rollback must restore exactly the durable bindings",
        backend.label
    );
    let rolled_back_snapshot = runtime
        .snapshot_execution_state()
        .await
        .expect("snapshot")
        .expect("execution state");
    assert_eq!(rolled_back_snapshot, snapshot, "{}", backend.label);

    let run = runtime
        .run_turn_assembled(
            TurnInput::text("go"),
            tokio_util::sync::CancellationToken::new(),
            turn_scope(&runtime, "fig2521-after-rollback"),
        )
        .await
        .unwrap_or_else(|error| panic!("{}: turn after rollback: {error:?}", backend.label));
    assert!(
        matches!(run.outcome, TurnOutcome::Finished(_)),
        "{:?}",
        run.outcome
    );
    let last_request = script.requests.lock_recover().last().cloned().unwrap();
    assert_eq!(
        count(&last_request, "projected_discarded"),
        0,
        "{}: the discarded binding must not reach the next prompt: {last_request}",
        backend.label
    );
    assert_eq!(
        count(&last_request, "`projected_original`"),
        1,
        "{last_request}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_follow_on_failure_then_resident_reload_rebinds_the_frame_seed_on_memory() {
    Box::pin(follow_on_failure_then_resident_reload(Backend::memory())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_follow_on_failure_then_resident_reload_rebinds_the_frame_seed_on_sqlite() {
    Box::pin(follow_on_failure_then_resident_reload(Backend::sqlite())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_follow_on_failure_then_resident_reload_rebinds_the_frame_seed_on_postgres() {
    if let Some(backend) = Backend::postgres().await {
        Box::pin(follow_on_failure_then_resident_reload(backend)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_reopen_seed_receipt_replay_reloads_the_same_frame_on_memory() {
    Box::pin(reopen_seed_receipt_replay(Backend::memory())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_reopen_seed_receipt_replay_reloads_the_same_frame_on_sqlite() {
    Box::pin(reopen_seed_receipt_replay(Backend::sqlite())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_reopen_seed_receipt_replay_reloads_the_same_frame_on_postgres() {
    if let Some(backend) = Backend::postgres().await {
        Box::pin(reopen_seed_receipt_replay(backend)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_faulted_append_rollback_discards_the_unpersisted_binding_on_memory() {
    Box::pin(faulted_append_rollback(Backend::memory())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_faulted_append_rollback_discards_the_unpersisted_binding_on_sqlite() {
    Box::pin(faulted_append_rollback(Backend::sqlite())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_faulted_append_rollback_discards_the_unpersisted_binding_on_postgres() {
    if let Some(backend) = Backend::postgres().await {
        Box::pin(faulted_append_rollback(backend)).await;
    }
}
