use super::*;
use lash_core::facade_support::RuntimeSessionStateFacadeOps;

use lash_core::{EffectReplayOwnership, ToolCall, ToolProvider};
use std::sync::atomic::{AtomicUsize, Ordering};

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

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolResult {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(call).await
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

fn lashlang_source_for(tool_name: &str) -> &'static str {
    match tool_name {
        "llm_query" => {
            r#"<lashlang>
result = await llm.query({
  task: "Return the covered answer",
  inputs: { answer: "covered" },
  output: Type { answer: str }
})?
finish result
</lashlang>"#
        }
        other => panic!(
            "first-party tool `{other}` was registered without a production Lashlang fixture; add its caller path before merging"
        ),
    }
}

struct ProductionToolCell {
    _dir: tempfile::TempDir,
    session_id: String,
    turn_id: String,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    runtime_store: Arc<dyn lash_core::RuntimePersistence>,
    replay_store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    tool_executions: Arc<AtomicUsize>,
    llm_provider_calls: Arc<AtomicUsize>,
}

impl ProductionToolCell {
    async fn new(replay_ownership: EffectReplayOwnership, tool_name: &str) -> Self {
        let context_name = match replay_ownership {
            EffectReplayOwnership::Runtime => "inline",
            EffectReplayOwnership::Controller => "restate-durable",
        };
        let session_id = format!("tool-context-{context_name}-{tool_name}");
        let turn_id = format!("{session_id}-turn");
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
                lash_protocol_rlm::RlmProtocolPluginConfig::new(
                    lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
                    lash_protocol_rlm::ExecutionBound::secs(30),
                    lash_protocol_rlm::ExecutionBound::instructions(64 * 1024 * 1024),
                ),
                artifact_store,
            )
            .with_process_lifecycle(false),
        );
        let plugin_factories = vec![rlm_plugin, tool_plugin];

        let llm_provider_calls = Arc::new(AtomicUsize::new(0));
        let source = lashlang_source_for(tool_name).to_string();
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
                            full_text: text.clone(),
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
        let replay_store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(
            lash_sqlite_store::Store::open(&dir.path().join("replay-session.db"))
                .await
                .expect("open production-path replay session store"),
        );
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
            replay_store,
            plugin_factories,
            tool_executions,
            llm_provider_calls,
        }
    }

    async fn run_once(
        &self,
        runtime: &mut lash_core::facade_support::LashRuntime,
        controller: &dyn RuntimeEffectController,
    ) -> lash_core::facade_support::AssembledTurn {
        let turn_scope = runtime.export_persistence_state().turn_scope(&self.turn_id);
        let scoped_effect_controller =
            lash_core::ScopedEffectController::borrowed(controller, turn_scope)
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
            .expect("run production tool cell")
    }

    async fn run(&self, controller: &dyn RuntimeEffectController, start_replay: impl FnOnce()) {
        let mut live = replay_test_runtime_with_plugins(
            &self.session_id,
            self.policy.clone(),
            self.initial_state.clone(),
            self.host.clone(),
            Arc::clone(&self.runtime_store),
            self.plugin_factories.clone(),
        )
        .await;
        let live_turn = self.run_once(&mut live, controller).await;
        assert!(matches!(
            live_turn.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ));
        assert_eq!(
            self.tool_executions.load(Ordering::SeqCst),
            1,
            "the real caller must execute the first-party tool once on the live pass"
        );

        start_replay();
        let mut replay = replay_test_runtime_with_plugins(
            &self.session_id,
            self.policy.clone(),
            self.initial_state.clone(),
            self.host.clone(),
            Arc::clone(&self.replay_store),
            self.plugin_factories.clone(),
        )
        .await;
        let replay_turn = self.run_once(&mut replay, controller).await;
        assert!(matches!(
            replay_turn.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ));
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

        let inline_cell =
            ProductionToolCell::new(EffectReplayOwnership::Runtime, &manifest.name).await;
        inline_cell
            .runtime_store
            .admit_and_bind_session(&lash_core::SessionBinding::root(
                inline_cell.session_id.clone(),
            ))
            .await
            .expect("bind inline session");
        let inline = lash_sqlite_store::SqliteRuntimeEffectController::memory(
            ExecutionScope::turn(&inline_cell.session_id, &inline_cell.turn_id),
        )
        .await
        .expect("in-process production replay controller");
        inline_cell.run(&inline, || inline.start_replay()).await;

        let durable_cell =
            ProductionToolCell::new(EffectReplayOwnership::Controller, &manifest.name).await;
        let context = Arc::new(ReplayableRecordingContext::default());
        let durable = RestateRuntimeEffectController::new(Arc::clone(&context));
        durable_cell.run(&durable, || context.start_replay()).await;
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
