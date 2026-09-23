use super::*;
use lash_core::facade_support::RuntimeSessionStateFacadeOps;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

use lash_core::{ToolCall, ToolProvider};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountingFirstPartyProvider {
    inner: Arc<dyn ToolProvider>,
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for CountingFirstPartyProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.inner.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.inner.resolve_contract(name)
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.inner.execute(call).await
        })
        .await
    }
}

fn args_for(tool_name: &str) -> serde_json::Value {
    match tool_name {
        "llm_query" => serde_json::json!({
            "task": "Return the covered answer",
            "inputs": {"answer": "covered"},
            "output": {"answer": "str"}
        }),
        other => panic!(
            "first-party tool `{other}` was registered without a conformance fixture; add its arguments before merging"
        ),
    }
}

fn typescript_source_for(tool_name: &str) -> &'static str {
    match tool_name {
        "llm_query" => {
            r#"<typescript>
const result = await llm.query({
  task: "Return the covered answer",
  inputs: { answer: "covered" },
  output: { answer: "str" }
});
finish(result);
</typescript>"#
        }
        other => panic!(
            "first-party tool `{other}` was registered without a production TypeScript fixture; add its caller path before merging"
        ),
    }
}

/// The live pass's store refuses every commit, so its worker dies at its final commit: every effect ran and was
/// journaled, and nothing was committed.
struct RefusesEveryCommitStore {
    inner: Arc<dyn lash_core::RuntimePersistence>,
}

#[async_trait::async_trait]
impl lash_core::store::RuntimePersistenceDecorator for RefusesEveryCommitStore {
    fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        _commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
        Err(lash_core::StoreError::Backend(
            "the live worker died at its final commit".to_string(),
        ))
    }
}

struct ProductionToolCell {
    _dir: tempfile::TempDir,
    session_id: SessionId,
    turn_id: TurnId,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    runtime_store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    tool_executions: Arc<AtomicUsize>,
    llm_provider_calls: Arc<AtomicUsize>,
}

enum ControllerMode {
    Local,
    Durable,
}

impl ProductionToolCell {
    async fn new(mode: ControllerMode, tool_name: &str) -> Self {
        let context_name = match mode {
            ControllerMode::Local => "local",
            ControllerMode::Durable => "restate-durable",
        };
        let session_id = SessionId::from(format!("tool-context-{context_name}-{tool_name}"));
        let turn_id = TurnId::from(format!("{session_id}-turn"));
        let dir = tempfile::tempdir().expect("tool-context tempdir");
        let first_party: Arc<dyn ToolProvider> =
            Arc::new(lash_llm_tools::llm_query_provider(None, None, None));
        let tool_executions = Arc::new(AtomicUsize::new(0));
        let counting_provider: Arc<dyn ToolProvider> = Arc::new(CountingFirstPartyProvider {
            inner: first_party,
            executions: Arc::clone(&tool_executions),
        });
        let tool_plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
            Arc::new(lash_core::plugin::StaticPluginFactory::new(
                "tool-context-first-party",
                lash_core::facade_support::PluginSpec::new().with_tool_provider(counting_provider),
            ));
        let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
            Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
        let rlm_plugin: Arc<dyn lash_core::facade_support::PluginFactory> = Arc::new(
            lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash_protocol_rlm::RlmChannel::Cell)
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                    .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build(),
                artifact_store,
            )
            .with_process_lifecycle(false),
        );
        let plugin_factories = vec![rlm_plugin, tool_plugin];

        let llm_provider_calls = Arc::new(AtomicUsize::new(0));
        let source = typescript_source_for(tool_name).to_string();
        let provider = lash_core::testing::TestProvider::builder()
            .kind("stub")
            .complete({
                let llm_provider_calls = Arc::clone(&llm_provider_calls);
                move |_request| {
                    let llm_provider_calls = Arc::clone(&llm_provider_calls);
                    let source = source.clone();
                    async move {
                        let call = llm_provider_calls.fetch_add(1, Ordering::SeqCst);
                        let text = match call {
                            0 => source,
                            1 => r#"{"kind":"value","value":{"answer":"covered"},"error":null}"#
                                .to_string(),
                            other => panic!(
                                "live+replay must not execute an unjournaled provider call #{other}"
                            ),
                        };
                        Ok(lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::Text {
                                text,
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        })
                    }
                }
            })
            .build()
            .into_handle();
        let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        );
        host.providers.provider_resolver = Arc::new(
            lash_core::facade_support::SingleProviderResolver::new(provider),
        );
        host.durability.attachment_store = Arc::new(
            lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
                DurableMemoryAttachmentStore::default(),
            )),
        );
        host.durability.process_env_store = Arc::new(DurableMemoryProcessEnvStore::default());

        let store = Arc::new(
            lash_sqlite_store::Store::open(&dir.path().join("session.db"))
                .await
                .expect("open production-path session store"),
        );
        let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store;
        let policy = replay_test_policy(&session_id);
        let initial_state = replay_test_state(&session_id, &policy);
        Self {
            _dir: dir,
            session_id,
            turn_id,
            policy,
            initial_state,
            host,
            runtime_store,
            plugin_factories,
            tool_executions,
            llm_provider_calls,
        }
    }

    async fn run_once(
        &self,
        runtime: &mut lash_core::facade_support::LashRuntime,
        effect_host: &dyn EffectHost,
    ) -> Result<lash_core::facade_support::AssembledTurn, lash_core::RuntimeError> {
        let turn_scope = runtime.export_persistence_state().turn_scope(&self.turn_id);
        let scoped_effect_controller = effect_host
            .scoped(durable_admission(&turn_scope))
            .expect("scope production tool cell");
        runtime
            .stream_turn(
                replay_test_input(&self.turn_id),
                lash_core::facade_support::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scoped_effect_controller,
                ),
            )
            .await
    }

    async fn run(&self, effect_host: &dyn EffectHost, start_replay: impl FnOnce()) {
        let mut live = replay_test_runtime_with_plugins(
            &self.session_id,
            self.policy.clone(),
            self.initial_state.clone(),
            self.host.clone(),
            Arc::new(RefusesEveryCommitStore {
                inner: Arc::clone(&self.runtime_store),
            }),
            self.plugin_factories.clone(),
        )
        .await;
        self.run_once(&mut live, effect_host)
            .await
            .expect_err("the live worker dies at its final commit");
        let live_state = live
            .snapshot_execution_state()
            .await
            .expect("snapshot live execution state");
        assert_binds_result_global(live_state.as_ref());
        assert_eq!(
            self.tool_executions.load(Ordering::SeqCst),
            1,
            "the real caller must execute the first-party tool once on the live pass"
        );

        // The replay is the same handler redriven against the same store: it
        // drives the journaled acceptance and drive set (ADR 0069 §6), replays
        // every journaled effect, and commits the turn the live pass could not.
        start_replay();
        let mut replay = replay_test_runtime_with_plugins(
            &self.session_id,
            self.policy.clone(),
            self.initial_state.clone(),
            self.host.clone(),
            Arc::clone(&self.runtime_store),
            self.plugin_factories.clone(),
        )
        .await;
        let replay_turn = self
            .run_once(&mut replay, effect_host)
            .await
            .expect("the replay commits the production tool cell");
        assert!(matches!(
            replay_turn.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ));
        let replay_state = replay
            .snapshot_execution_state()
            .await
            .expect("snapshot replayed execution state");
        assert_eq!(
            replay_state, live_state,
            "replay re-runs the code cell, so it rebuilds the live pass's interpreter state"
        );
        assert_eq!(
            self.tool_executions.load(Ordering::SeqCst),
            1,
            "the caller-emitted ToolAttempt must replay without re-executing the first-party tool"
        );
        assert_eq!(
            self.llm_provider_calls.load(Ordering::SeqCst),
            2,
            "outer RLM generation and llm_query direct completion must each execute only on the live pass"
        );
    }
}

#[tokio::test]
async fn every_registered_first_party_tool_succeeds_and_replays_in_every_context() {
    let provider: Arc<dyn ToolProvider> =
        Arc::new(lash_llm_tools::llm_query_provider(None, None, None));
    let manifests = provider.tool_manifests();
    assert!(
        !manifests.is_empty(),
        "the first-party tool registry must not be empty"
    );

    for manifest in manifests {
        let _ = args_for(&manifest.name);

        let (local_cell, local) = ProductionToolCell::sqlite(&manifest.name).await;
        local_cell
            .run(local.as_ref(), || local.start_replay())
            .await;

        let mut durable_cell =
            ProductionToolCell::new(ControllerMode::Durable, &manifest.name).await;
        let context = Arc::new(ReplayableRecordingContext::default());
        let durable = Arc::new(RestateRuntimeEffectController::new_for_test(Arc::clone(
            &context,
        )));
        durable_cell.host.control.effect_host = Arc::clone(&durable) as Arc<dyn EffectHost>;
        durable_cell
            .run(durable.as_ref(), || context.start_replay())
            .await;
        let tool_attempts = context
            .recorded_runtime_effect_envelopes()
            .into_iter()
            .filter(|(_, envelope)| {
                matches!(
                    &envelope.command,
                    RuntimeEffectCommand::ToolAttempt { call, .. }
                        if call.tool_name == manifest.name
                )
            })
            .count();
        assert_eq!(
            tool_attempts, 1,
            "the production durable caller must emit one ToolAttempt for {}",
            manifest.name
        );
    }
}

/// The production cell binds `const result`, so its interpreter state names it.
fn assert_binds_result_global(state: Option<&lash_core::plugin::HydratedExecutionState>) {
    let state = state.expect("an RLM turn leaves execution state");
    assert!(
        String::from_utf8_lossy(&state.root).contains("result"),
        "the cell's `result` global must be in the execution state root"
    );
}

/// Fails the first turn-final commit before it reaches the store: the crash
/// window between the last journaled effect and the durable head.
struct CrashAtFinalCommit {
    inner: Arc<dyn lash_core::RuntimePersistence>,
    armed: AtomicBool,
}

#[async_trait::async_trait]
impl lash_core::store::RuntimePersistenceDecorator for CrashAtFinalCommit {
    fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
        if commit.turn_commit.operation.key == "final" && self.armed.swap(false, Ordering::SeqCst) {
            return Err(lash_core::StoreError::Backend(
                "injected crash at the turn-final commit".to_string(),
            ));
        }
        self.inner.commit_runtime_state(commit).await
    }
}

impl ProductionToolCell {
    /// A cell on the SQLite effect host with its session bound, ready to run.
    async fn sqlite(tool_name: &str) -> (Self, Arc<lash_sqlite_store::SqliteEffectHost>) {
        let mut cell = Self::new(ControllerMode::Local, tool_name).await;
        cell.runtime_store
            .admit_and_bind_session(&lash_core::SessionBinding::root(cell.session_id.clone()))
            .await
            .expect("bind local session");
        let host = Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(&cell._dir.path().join("effects.db"))
                .await
                .expect("in-process production replay host"),
        );
        cell.host.control.effect_host = Arc::clone(&host) as Arc<dyn EffectHost>;
        (cell, host)
    }

    async fn runtime_on(
        &self,
        store: Arc<dyn lash_core::RuntimePersistence>,
    ) -> lash_core::facade_support::LashRuntime {
        replay_test_runtime_with_plugins(
            &self.session_id,
            self.policy.clone(),
            self.initial_state.clone(),
            self.host.clone(),
            store,
            self.plugin_factories.clone(),
        )
        .await
    }

    /// The execution state of the store's durable head, restored the way a
    /// fresh process restores it: from the head, with no caller-supplied state.
    async fn committed_execution_state(&self) -> Option<lash_core::plugin::HydratedExecutionState> {
        let mut runtime = Box::pin(
            lash_core::facade_support::LashRuntime::builder(
                lash_core::CommitBudget::bounded(1024 * 1024, 512),
                lash_core::QueuedWorkBatchingConfig::new(1),
                lash_core::LeaseOwnerIdentity::opaque(
                    "lash-restate-head-reader",
                    "lash-restate-head-reader-boot",
                ),
            )
            .with_session_id(&self.session_id)
            .with_policy(self.policy.clone())
            .with_runtime_host(self.host.clone())
            .with_plugin_factories(self.plugin_factories.clone())
            .with_store(Arc::clone(&self.runtime_store))
            .build(),
        )
        .await
        .expect("open a runtime on the committed head");
        runtime
            .snapshot_execution_state()
            .await
            .expect("snapshot committed execution state")
    }
}

/// FIG-3549: a crash between the last journaled effect and the turn-final
/// commit, then a same-store redrive, commits the live pass's interpreter
/// state. The redrive re-runs the code cell (ADR 0103); the cell's nested
/// `llm_query` answers from the journal, so no provider call is re-issued.
#[tokio::test]
async fn redrive_after_a_crash_at_final_commit_commits_the_live_execution_state() {
    let (control, control_host) = ProductionToolCell::sqlite("llm_query").await;
    let mut control_runtime = control.runtime_on(Arc::clone(&control.runtime_store)).await;
    let control_turn = control
        .run_once(&mut control_runtime, control_host.as_ref())
        .await
        .expect("the control turn commits");
    assert!(matches!(
        control_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    drop(control_runtime);
    let live_head = control.committed_execution_state().await;
    assert_binds_result_global(live_head.as_ref());

    let (cell, host) = ProductionToolCell::sqlite("llm_query").await;
    let crashing: Arc<dyn lash_core::RuntimePersistence> = Arc::new(CrashAtFinalCommit {
        inner: Arc::clone(&cell.runtime_store),
        armed: AtomicBool::new(true),
    });
    let mut crashed = cell.runtime_on(Arc::clone(&crashing)).await;
    let turn_scope = crashed.export_persistence_state().turn_scope(&cell.turn_id);
    let crashed_turn = crashed
        .stream_turn(
            replay_test_input(&cell.turn_id),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                host.scoped(durable_admission(&turn_scope))
                    .expect("scope crashing tool cell"),
            ),
        )
        .await;
    assert!(
        !matches!(
            crashed_turn.as_ref().map(|turn| &turn.outcome),
            Ok(lash_core::facade_support::TurnOutcome::Finished(_))
        ),
        "the injected crash must stop the turn-final commit"
    );
    drop(crashed);
    assert_eq!(cell.tool_executions.load(Ordering::SeqCst), 1);
    assert_eq!(cell.llm_provider_calls.load(Ordering::SeqCst), 2);

    host.start_replay();
    let mut redrive = cell.runtime_on(Arc::clone(&cell.runtime_store)).await;
    let redriven = cell
        .run_once(&mut redrive, host.as_ref())
        .await
        .expect("the redrive commits");
    assert!(matches!(
        redriven.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        redrive
            .snapshot_execution_state()
            .await
            .expect("snapshot redriven execution state"),
        live_head,
        "the redrive re-runs the cell and holds the live interpreter state"
    );
    drop(redrive);
    assert_eq!(
        cell.committed_execution_state().await,
        live_head,
        "the redriven turn commits the live pass's execution state byte for byte"
    );
    assert_eq!(
        cell.tool_executions.load(Ordering::SeqCst),
        1,
        "the cell's ToolAttempt replays from the journal"
    );
    assert_eq!(
        cell.llm_provider_calls.load(Ordering::SeqCst),
        2,
        "the redrive re-issues neither the outer generation nor the nested llm_query call"
    );
}
