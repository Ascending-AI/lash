//! Same-frame `restore_session` on the real RLM plugin is idempotent
//! (FIG-2521).
//!
//! Every runtime path that restores the protocol session on the frame the
//! plugin already holds — resident reload after a follow-on failure, reopen-seed
//! receipt replay, and append rollback — must rebuild execution state and
//! projected bindings from the restore view instead of rejecting the seed the
//! plugin already bound, and nothing the failed operation left in the live
//! execution (a never-persisted binding or global, a follow-on turn's
//! assignment whose commit was refused) may reach the next prompt, the next
//! execution result, or the next durable checkpoint. Each path is witnessed on
//! the memory, SQLite and PostgreSQL backends with the shipped RLM plugin, not
//! a test protocol.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::facade_support::{
    EmbeddedRuntimeHost, InMemorySessionStoreFactory, LashRuntime, NativeRuntimeEffectController,
    PersistentRuntimeServices, PluginHost, PluginSession, RuntimeHostConfig,
    SingleProviderResolver, TurnFinish, TurnOutcome,
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

/// A seed defaulting the globals `payload` and `baton` and binding the
/// projected entry `projected_<label>`.
fn seed(label: &str) -> RlmSeed {
    let mut seed = RlmSeed::from_seed_value(&serde_json::json!({
        "payload": (0..64).collect::<Vec<_>>(),
        "baton": label,
    }))
    .expect("seed value");
    seed.projected.push(
        format!("projected_{label}"),
        lash_rlm_types::RlmProjectedSeedEntry::Materialized(serde_json::json!({ "value": label })),
    );
    seed
}

fn seed_nodes(label: &str) -> Vec<SessionAppendNode> {
    rlm_seed_initial_nodes(seed(label))
}

fn lashlang_block(code: &str) -> String {
    format!("<lashlang>\n{code}\n</lashlang>")
}

/// The persisted RLM snapshot root, decoded far enough to name its globals and
/// read one back through its inline body or leaf component.
#[derive(Debug, serde::Deserialize)]
struct RlmExecutionSnapshotRoot {
    globals: std::collections::BTreeMap<String, RlmPersistedValueProbe>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RlmPersistedValueProbe {
    Inline {
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    Leaf {
        component: String,
    },
}

/// The global names and one named global's rendered value in an execution
/// snapshot.
fn snapshot_globals(
    state: &lash_core::plugin::HydratedExecutionState,
    name: &str,
) -> (Vec<String>, Option<String>) {
    let root: RlmExecutionSnapshotRoot =
        rmp_serde::from_slice(&state.root).expect("decode the RLM snapshot root");
    let value = root.globals.get(name).map(|persisted| {
        let body = match persisted {
            RlmPersistedValueProbe::Inline { body } => body.as_slice(),
            RlmPersistedValueProbe::Leaf { component } => state
                .components
                .get(component)
                .unwrap_or_else(|| panic!("leaf `{component}` must be hydrated"))
                .as_slice(),
        };
        let snapshot = lashlang::Snapshot::from_canonical_bytes(body).expect("decode the global");
        format!("{:?}", snapshot.globals().get("value"))
    });
    (root.globals.keys().cloned().collect(), value)
}

/// The execution root the durable head carries right now, if any.
async fn durable_execution_state(
    store: &FaultStore,
) -> Option<lash_core::plugin::HydratedExecutionState> {
    lash_core::store::load_persisted_session_state(store.inner.as_ref())
        .await
        .expect("load the durable head")
        .expect("the session is persisted")
        .execution_state_hydration()
        .expect("hydrate the durable execution state")
}

/// The global names and `baton`'s rendered value in the durable checkpoint.
async fn durable_globals(store: &FaultStore, name: &str) -> (Vec<String>, Option<String>) {
    let state = durable_execution_state(store)
        .await
        .expect("the durable head carries an execution root");
    snapshot_globals(&state, name)
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

/// (c) A faulted append rolls the protocol session back on the same frame:
/// the never-persisted `projected_discarded` binding and the never-persisted
/// `rolled_back_global` global must not survive into the next prompt, the next
/// execution result, or the next durable checkpoint.
async fn faulted_append_rollback(backend: Backend) {
    let script = Arc::new(Script {
        responses: vec![lashlang_block("finish baton")],
        ..Script::default()
    });
    let SeededSession {
        mut runtime,
        plugins,
        store,
        snapshot,
        prompt,
    } = Box::pin(backend.seeded_session("append-rollback", Arc::clone(&script))).await;
    assert!(
        runtime
            .export_persistence_state()
            .execution_state_hydration()
            .expect("hydration")
            .is_none(),
        "{}: the rollback must run without a resident execution body (discarded post-commit)",
        backend.label
    );

    // The discarded seed carries a global the durable history never names, so
    // a rollback that keeps the live execution is visible as a global with no
    // originating event.
    let mut discarded = seed("discarded");
    discarded.globals.insert(
        "rolled_back_global".to_string(),
        serde_json::json!("UNCOMMITTED-GLOBAL"),
    );
    store.arm(CommitFault::FailNext);
    let error = runtime
        .append_session_nodes(AppendSessionNodesRequest {
            operation_id: "fig2521-discarded".to_string(),
            requires_ancestor_node_id: None,
            nodes: rlm_seed_initial_nodes(discarded),
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
    assert_eq!(
        rolled_back_snapshot, snapshot,
        "{}: rollback must restore exactly the committed execution",
        backend.label
    );
    let (live_globals, _) = snapshot_globals(&rolled_back_snapshot, "baton");
    assert!(
        !live_globals.iter().any(|name| name == "rolled_back_global"),
        "{}: the rolled-back append's global must not survive in the live execution: {live_globals:?}",
        backend.label
    );

    let run = runtime
        .run_turn_assembled(
            TurnInput::text("go"),
            tokio_util::sync::CancellationToken::new(),
            turn_scope(&runtime, "fig2521-after-rollback"),
        )
        .await
        .unwrap_or_else(|error| panic!("{}: turn after rollback: {error:?}", backend.label));
    assert_eq!(
        run.outcome,
        TurnOutcome::Finished(TurnFinish::FinalValue {
            value: serde_json::json!("original")
        }),
        "{}: the next execution must see the committed globals only",
        backend.label
    );
    let last_request = script.requests.lock_recover().last().cloned().unwrap();
    assert_eq!(
        count(&last_request, "projected_discarded"),
        0,
        "{}: the discarded binding must not reach the next prompt: {last_request}",
        backend.label
    );
    assert_eq!(
        count(&last_request, "rolled_back_global"),
        0,
        "{}: the rolled-back global must not reach the next prompt: {last_request}",
        backend.label
    );
    assert_eq!(
        count(&last_request, "`projected_original`"),
        1,
        "{last_request}"
    );
    let (durable_names, durable_baton) = durable_globals(&store, "baton").await;
    assert!(
        !durable_names
            .iter()
            .any(|name| name == "rolled_back_global"),
        "{}: the rolled-back global must not become durable: {durable_names:?}",
        backend.label
    );
    assert_eq!(
        durable_baton.as_deref(),
        Some("Some(String(\"original\"))"),
        "{}",
        backend.label
    );
}

/// (d) A follow-on turn assigns a global, then its commit is refused; the
/// invalidated resident state reloads on the frame the plugin already holds,
/// whose durable head carries no execution root (the frame switch cleared it).
/// The next execution must see the committed seed value, never the refused
/// turn's assignment, and the next durable checkpoint must persist the
/// committed value.
async fn follow_on_failure_discards_the_uncommitted_execution(backend: Backend) {
    let script = Arc::new(Script {
        responses: vec![
            continue_as_response(),
            lashlang_block("baton = \"UNCOMMITTED-FOLLOW-ON\"\nfinish baton"),
            lashlang_block("finish baton"),
        ],
        ..Script::default()
    });
    *script.arm_before_call.lock_recover() = Some((1, CommitFault::FailNext));
    let SeededSession {
        mut runtime, store, ..
    } = Box::pin(backend.seeded_session("follow-on-execution", Arc::clone(&script))).await;

    let run = runtime
        .run_turn_assembled(
            TurnInput::text("switch"),
            tokio_util::sync::CancellationToken::new(),
            turn_scope(&runtime, "fig2521-switch-mutating"),
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
    assert_eq!(*store.fault.lock_recover(), CommitFault::None);
    assert!(
        durable_execution_state(&store).await.is_none(),
        "{}: the switch commit leaves the new frame without a durable execution root",
        backend.label
    );

    let run = runtime
        .run_turn_assembled(
            TurnInput::text("read"),
            tokio_util::sync::CancellationToken::new(),
            turn_scope(&runtime, "fig2521-after-follow-on-failure"),
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}: resident reload on the same frame must succeed: {error:?}",
                backend.label
            )
        });
    assert_eq!(script.calls.load(Ordering::SeqCst), 3, "{}", backend.label);
    assert_eq!(
        run.outcome,
        TurnOutcome::Finished(TurnFinish::FinalValue {
            value: serde_json::json!("switched")
        }),
        "{}: the reloaded execution must hold the committed seed value, not the refused \
         turn's assignment",
        backend.label
    );
    let last_request = script.requests.lock_recover().last().cloned().unwrap();
    assert_eq!(
        count(&last_request, "UNCOMMITTED-FOLLOW-ON"),
        0,
        "{}: {last_request}",
        backend.label
    );
    let (durable_names, durable_baton) = durable_globals(&store, "baton").await;
    assert_eq!(
        durable_baton.as_deref(),
        Some("Some(String(\"switched\"))"),
        "{}: the next checkpoint must persist the committed value: {durable_names:?}",
        backend.label
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_follow_on_failure_discards_the_uncommitted_execution_on_memory() {
    Box::pin(follow_on_failure_discards_the_uncommitted_execution(
        Backend::memory(),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_follow_on_failure_discards_the_uncommitted_execution_on_sqlite() {
    Box::pin(follow_on_failure_discards_the_uncommitted_execution(
        Backend::sqlite(),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_follow_on_failure_discards_the_uncommitted_execution_on_postgres() {
    if let Some(backend) = Backend::postgres().await {
        Box::pin(follow_on_failure_discards_the_uncommitted_execution(
            backend,
        ))
        .await;
    }
}
