#![expect(
    clippy::expect_used,
    reason = "runtime law fixture failures must stop the test"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::facade_support::{
    EmbeddedRuntimeHost, LashRuntime, PersistentRuntimeServices, PluginHost, RuntimeHostConfig,
    SingleProviderResolver,
};
use lash_core::plugin::{PluginFactory, RecordedSessionConfig};
use lash_core::testing::TestTurnDrive as _;
use lash_core::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentStore, AttachmentStoreError,
    AttachmentStorePersistence, CommitBudget, LlmOutputPart, LlmResponse, ModelSpec,
    QueuedWorkBatchingConfig, RuntimeSessionState, SessionCreationHead, SessionPolicy,
    SessionRelation, SessionStoreCreateRequest, StoredAttachment, StoredBlobRef,
    ToolAttemptOutcome, ToolCall, ToolCallOutput, ToolContract, ToolDefinition, ToolId,
    ToolManifest, ToolOutcomeDone, ToolProvider, TurnBudget, TurnInput,
};
use lash_protocol_standard::render::{ToolOutputRendererSlot, ToolRenderParams};
use lash_protocol_standard::{
    BuiltinToolOutputRenderer, StandardProtocolConfig, StandardProtocolPluginFactory,
    StandardRenderConfig, StandardTurnOptions, ToolOutputRenderer,
};
use lash_sansio::llm::types::{LlmContentBlock, LlmRequest};
use lash_sansio::{SessionId, TurnId};

struct SwitchingRenderer {
    mode: AtomicUsize,
    first: AtomicUsize,
    second: AtomicUsize,
    first_limit: AtomicUsize,
    second_limit: AtomicUsize,
}

impl ToolOutputRenderer for SwitchingRenderer {
    fn id(&self) -> &str {
        if self.mode.load(Ordering::SeqCst) == 0 {
            "law.first"
        } else {
            "law.second"
        }
    }

    fn tool_output(
        &self,
        output: &ToolCallOutput,
        tool: &ToolId,
        params: &ToolRenderParams,
    ) -> lash_render::Rendered<Vec<lash_core::facade_support::ModelToolReturnPart>> {
        if self.mode.load(Ordering::SeqCst) == 0 {
            self.first.fetch_add(1, Ordering::SeqCst);
            self.first_limit
                .store(params.value.max_chars, Ordering::SeqCst);
        } else {
            self.second.fetch_add(1, Ordering::SeqCst);
            self.second_limit
                .store(params.value.max_chars, Ordering::SeqCst);
        }
        BuiltinToolOutputRenderer.tool_output(output, tool, params)
    }
}

struct CountingAttachments {
    inner: Arc<dyn AttachmentStore>,
    puts: AtomicUsize,
    gets: AtomicUsize,
}

#[async_trait::async_trait]
impl AttachmentStore for CountingAttachments {
    fn persistence(&self) -> AttachmentStorePersistence {
        self.inner.persistence()
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(bytes, meta).await
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(id).await
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        self.inner.head(id).await
    }
}

fn tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "law:fixture",
        "fixture",
        "Return a long result",
        ToolDefinition::default_input_schema(),
        serde_json::json!({"type":"string"}),
    )
}

struct FixtureTool {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ToolProvider for FixtureTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "fixture").then(|| Arc::new(tool_definition().contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolAttemptOutcome {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let value = match call {
            1 => "short uncut".to_string(),
            0 => "a".repeat(800),
            _ => "b".repeat(800),
        };
        ToolAttemptOutcome::done_without_intents(ToolOutcomeDone::ok(serde_json::json!(value)))
    }
}

struct Script {
    calls: AtomicUsize,
    requests: Mutex<Vec<LlmRequest>>,
}

fn policy() -> SessionPolicy {
    SessionPolicy {
        provider_id: "standard-render-law".into(),
        model: ModelSpec::builder("standard-render-law-model")
            .context_window_tokens(100_000)
            .build()
            .expect("model spec"),
        ..SessionPolicy::new(TurnBudget::Unbounded)
    }
}

fn provider(script: Arc<Script>) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("standard-render-law")
        .complete(move |request| {
            let script = Arc::clone(&script);
            async move {
                let call = script.calls.fetch_add(1, Ordering::SeqCst);
                script.requests.lock().expect("request lock").push(request);
                let parts = match call {
                    0 => vec![0, 1]
                        .into_iter()
                        .map(|index| LlmOutputPart::ToolCall {
                            call_id: format!("fixture-0-{index}"),
                            tool_name: "fixture".into(),
                            input_json: "{}".into(),
                            replay: None,
                        })
                        .collect(),
                    2 | 4 => vec![LlmOutputPart::ToolCall {
                        call_id: format!("fixture-{call}"),
                        tool_name: "fixture".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    _ => vec![LlmOutputPart::Text {
                        text: "done".into(),
                        response_meta: None,
                    }],
                };
                Ok(LlmResponse {
                    parts,
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn render_options(max_chars: usize) -> lash_core::ProtocolTurnOptions {
    lash_core::ProtocolTurnOptions::typed(StandardTurnOptions {
        render: Some(StandardRenderConfig {
            defaults: lash_protocol_standard::render::ToolRenderPatch {
                value: lash_render::RenderParamsPatch {
                    max_chars: Some(max_chars),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }),
    })
    .expect("render options")
}

async fn open_runtime(
    backend: &lash_core::Backend,
    store: lash_core::store::SessionStore,
    script: Arc<Script>,
    state: RuntimeSessionState,
    renderer: Arc<SwitchingRenderer>,
    attachments: Arc<CountingAttachments>,
    tool: Arc<FixtureTool>,
) -> LashRuntime {
    let mut config = StandardProtocolConfig::default();
    config.render.defaults.value.max_chars = Some(160);
    config.renderer = ToolOutputRendererSlot(renderer);
    let factories: Vec<Arc<dyn PluginFactory>> = vec![
        Arc::new(StandardProtocolPluginFactory::with_config(config)),
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "render-law-tool",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tool),
        )),
    ];
    let host = PluginHost::new(factories);
    let plugins = if let Some(snapshot) = state.plugin_state() {
        host.rematerialize_session(
            &state.session_id,
            snapshot,
            RecordedSessionConfig::new(state.protocol_turn_options.clone()),
        )
        .expect("rematerialize standard plugin")
    } else {
        host.build_session(&state.session_id)
            .expect("build standard plugin")
    };
    let mut host_config = RuntimeHostConfig::new(
        backend.clone(),
        CommitBudget::bounded(8 * 1024 * 1024, 1024),
        QueuedWorkBatchingConfig::new(1),
    );
    host_config.providers.provider_resolver =
        Arc::new(SingleProviderResolver::new(provider(script)));
    let mut runtime_host = EmbeddedRuntimeHost::new(host_config);
    runtime_host.core.durability.attachment_store =
        Arc::new(lash_core::facade_support::SessionAttachmentStore::new(
            attachments.clone(),
            Arc::new(
                lash_core::testing::conformance_support::PersistenceManifestAdapter(Arc::clone(
                    store.store(),
                )),
            ),
            state.session_id.clone(),
        ));
    let services = PersistentRuntimeServices::new(
        plugins,
        store,
        Arc::clone(&runtime_host.core.durability.attachment_store),
        Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        policy(),
        runtime_host,
        services,
        state,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("open runtime");
    runtime
        .configure_protocol_on_materialize(&lash_core::PluginOptions::empty(), true)
        .expect("materialize protocol");
    runtime
}

async fn drive(
    runtime: &mut LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    session_id: &SessionId,
    id: &str,
) {
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(session_id, TurnId::from(id)))
        .await
        .expect("open turn handler");
    let result = runtime
        .drive_turn(
            TurnInput::text(id),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                handler.scoped(),
            ),
        )
        .await
        .expect("drive turn");
    handler.close().await.expect("close turn handler");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
}

async fn drive_with_run_spec(
    runtime: &mut LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    store: &lash_core::store::SessionStore,
    session_id: &SessionId,
) {
    store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::next_turn(),
                TurnInput::text("run-spec"),
            )
            .with_source_key("render-law-run-spec")
            .with_run_spec(lash_core::RunSpec::overrides(lash_core::RunOverrides {
                protocol_turn_options: Some(render_options(100)),
                ..Default::default()
            })),
        )
        .await
        .expect("enqueue run spec");
    let handler = double
        .open_handler(lash_core::AdmittedScope::queue_drain(
            session_id.clone(),
            TurnId::from("run-spec"),
        ))
        .await
        .expect("open drive handler");
    let build_generation = runtime.host.core.backend().build_generation().clone();
    let outcome = lash_core::drive::drive_session(
        runtime,
        &handler.scoped(),
        &lash_core::engine::DriveRequest {
            session: session_id.clone(),
            request: lash_core::engine::DriveRequestId::new("render-law-run-spec"),
            build_generation,
        },
    )
    .await
    .expect("drive run spec");
    handler.close().await.expect("close drive handler");
    assert_eq!(outcome.ran.len(), 1);
}

fn history_prefix(request: &LlmRequest) -> Vec<lash_sansio::llm::types::LlmMessage> {
    let last = request
        .messages
        .iter()
        .rposition(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    LlmContentBlock::Text {
                        cache_breakpoint: true,
                        ..
                    }
                )
            })
        })
        .unwrap_or_else(|| {
            request
                .messages
                .len()
                .checked_sub(1)
                .expect("history messages")
        });
    request.messages[..=last].to_vec()
}

fn stable_bytes(messages: &[lash_sansio::llm::types::LlmMessage]) -> Vec<u8> {
    let mut messages = messages.to_vec();
    for message in &mut messages {
        for block in Arc::make_mut(&mut message.blocks).iter_mut() {
            if let LlmContentBlock::Text {
                cache_breakpoint, ..
            } = block
            {
                *cache_breakpoint = false;
            }
        }
    }
    serde_json::to_vec(&messages).expect("history JSON")
}

fn replay_presentation_outcome() -> lash_core::tool_dispatch::ToolDispatchOutcome {
    lash_core::tool_dispatch::ToolDispatchOutcome {
        record: lash_core::ToolCallRecord {
            call_id: lash_core::ToolCallId::fixture("replay-fixture"),
            provider_call_id: None,
            tool: "fixture".into(),
            args: serde_json::json!({}),
            output: ToolCallOutput::success(serde_json::json!("long value ".repeat(2000))),
        },
        attempts: Vec::new(),
        intents: lash_core::ToolIntents::default(),
        intent_outcomes: Vec::new(),
        captures: Vec::new(),
        triggers: Vec::new(),
    }
}

#[test]
fn journaled_standard_presentation_replays_without_render_or_retention_io() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let stores = lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite stores");
            let double = lash_restate_test::backend_with(
                0x3932_0002,
                lash_restate_test::ServerConfig::default(),
                move |_| Arc::new(stores),
            )
            .await
            .expect("Restate double");
            let backend = double.lash_backend();
            let session_id = SessionId::from("standard-presentation-replay-law");
            let store = lash_core::runtime::admit_session_view(
                &backend.session_store_factory(),
                &SessionStoreCreateRequest {
                    owning_process_id: None,
                    pending_observer_intents: Vec::new(),
                    session_id: session_id.clone(),
                    relation: SessionRelation::Root,
                    config: policy().into(),
                    head: SessionCreationHead::CommittedByCreator,
                },
            )
            .await
            .expect("create session store");
            let attachments = Arc::new(CountingAttachments {
                inner: backend.attachment_store(),
                puts: AtomicUsize::new(0),
                gets: AtomicUsize::new(0),
            });
            let facade = Arc::new(lash_core::facade_support::SessionAttachmentStore::new(
                attachments.clone(),
                Arc::new(
                    lash_core::testing::conformance_support::PersistenceManifestAdapter(
                        Arc::clone(store.store()),
                    ),
                ),
                session_id.clone(),
            ));
            let renderer = Arc::new(SwitchingRenderer {
                mode: AtomicUsize::new(0),
                first: AtomicUsize::new(0),
                second: AtomicUsize::new(0),
                first_limit: AtomicUsize::new(0),
                second_limit: AtomicUsize::new(0),
            });
            let recorded = lash_core::RecordedRender {
                renderer_id: "law.first".into(),
                params: serde_json::to_value(
                    lash_protocol_standard::render::resolve(
                        &StandardRenderConfig::builtin(),
                        &StandardRenderConfig::default(),
                        &StandardRenderConfig::default(),
                    )
                    .expect("resolve render"),
                )
                .expect("render parameters"),
            };
            let live_return = Arc::new(Mutex::new(None));
            let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
                let session_id = session_id.clone();
                let backend = backend.clone();
                let renderer = Arc::clone(&renderer);
                let attachments = Arc::clone(&attachments);
                let facade = Arc::clone(&facade);
                let live_return = Arc::clone(&live_return);
                let recorded = recorded.clone();
                Arc::new(move |scoped| {
                    let session_id = session_id.clone();
                    let backend = backend.clone();
                    let renderer = Arc::clone(&renderer);
                    let attachments = Arc::clone(&attachments);
                    let facade = Arc::clone(&facade);
                    let live_return = Arc::clone(&live_return);
                    let recorded = recorded.clone();
                    Box::pin(async move {
                        let config = StandardProtocolConfig {
                            renderer: ToolOutputRendererSlot(renderer.clone()),
                            ..StandardProtocolConfig::default()
                        };
                        let factories: Vec<Arc<dyn PluginFactory>> = vec![
                            Arc::new(StandardProtocolPluginFactory::with_config(config)),
                            Arc::new(lash_core::plugin::StaticPluginFactory::new(
                                "render-law-tool",
                                lash_core::facade_support::PluginSpec::new().with_tool_provider(
                                    Arc::new(FixtureTool {
                                        calls: AtomicUsize::new(0),
                                    }),
                                ),
                            )),
                        ];
                        let context =
                            lash_core::testing::TestExecutionContextBuilder::for_backend(&backend)
                                .session_id(session_id.clone())
                                .borrowed_effect_controller(scoped)
                                .plugin_factories(factories)
                                .attachment_store(facade)
                                .execution_env_spec({
                                    let mut spec = lash_core::ProcessExecutionEnvSpec::new(
                                        lash_core::PluginOptions::default(),
                                        policy(),
                                    );
                                    spec.render = Some(recorded);
                                    spec
                                })
                                .build()
                                .into_runtime();
                        let completed = context
                            .complete_tool_call(
                                lash_core::tool_dispatch::ToolCallIds {
                                    call_id: lash_core::ToolCallId::fixture("replay-fixture"),
                                    provider_call_id: None,
                                },
                                ToolId::new("law:fixture"),
                                None,
                                replay_presentation_outcome(),
                                "replay-fixture",
                                if crash { 46 } else { 2 },
                            )
                            .await
                            .expect("present result");
                        if crash {
                            assert_eq!(renderer.first.load(Ordering::SeqCst), 1);
                            assert_eq!(attachments.puts.load(Ordering::SeqCst), 1);
                            *live_return.lock().expect("live result lock") =
                                Some(completed.completed.model_return);
                            panic!("crash after presentation was journaled");
                        }
                        assert_eq!(renderer.first.load(Ordering::SeqCst), 1);
                        assert_eq!(attachments.puts.load(Ordering::SeqCst), 1);
                        assert_eq!(attachments.gets.load(Ordering::SeqCst), 0);
                        assert_eq!(
                            completed.completed.model_return,
                            live_return
                                .lock()
                                .expect("live result lock")
                                .clone()
                                .expect("live result")
                        );
                    })
                })
            };
            double
                .run_crashed_then_redriven(
                    lash_core::AdmittedScope::turn(&session_id, TurnId::from("replay-turn")),
                    attempt(true),
                    attempt(false),
                )
                .await
                .expect("redrive serves the recorded presentation");
        });
}

#[test]
fn standard_runtime_keeps_recorded_history_across_params_renderer_and_reopen() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let stores = lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite stores");
            let double = lash_restate_test::backend_with(
                0x3932_0001,
                lash_restate_test::ServerConfig::default(),
                move |_| Arc::new(stores),
            )
            .await
            .expect("Restate double");
            let backend = double.lash_backend();
            let session_id = SessionId::from("standard-render-cache-law");
            let store = lash_core::runtime::admit_session_view(
                &backend.session_store_factory(),
                &SessionStoreCreateRequest {
                    owning_process_id: None,
                    pending_observer_intents: Vec::new(),
                    session_id: session_id.clone(),
                    relation: SessionRelation::Root,
                    config: policy().into(),
                    head: SessionCreationHead::CommittedByCreator,
                },
            )
            .await
            .expect("create session store");
            let attachments = Arc::new(CountingAttachments {
                inner: backend.attachment_store(),
                puts: AtomicUsize::new(0),
                gets: AtomicUsize::new(0),
            });
            let renderer = Arc::new(SwitchingRenderer {
                mode: AtomicUsize::new(0),
                first: AtomicUsize::new(0),
                second: AtomicUsize::new(0),
                first_limit: AtomicUsize::new(0),
                second_limit: AtomicUsize::new(0),
            });
            let tool = Arc::new(FixtureTool {
                calls: AtomicUsize::new(0),
            });
            let script = Arc::new(Script {
                calls: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
            });
            let mut runtime = open_runtime(
                &backend,
                store.clone(),
                Arc::clone(&script),
                RuntimeSessionState {
                    session_id: session_id.clone(),
                    protocol_turn_options: render_options(140),
                    ..RuntimeSessionState::new(policy())
                },
                Arc::clone(&renderer),
                Arc::clone(&attachments),
                Arc::clone(&tool),
            )
            .await;
            drive(&mut runtime, &double, &session_id, "first").await;
            assert_eq!(script.calls.load(Ordering::SeqCst), 2);
            assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
            assert_eq!(renderer.first_limit.load(Ordering::SeqCst), 140);
            assert_eq!(attachments.puts.load(Ordering::SeqCst), 1);
            let prefix = {
                let requests = script.requests.lock().expect("requests");
                let history = history_prefix(requests.get(1).expect("post-tool request"));
                let bytes = stable_bytes(&history);
                assert!(
                    String::from_utf8_lossy(&bytes).contains("[output cut:"),
                    "{}",
                    String::from_utf8_lossy(&bytes)
                );
                assert!(String::from_utf8_lossy(&bytes).contains("short uncut"));
                bytes
            };
            let command = runtime.set_protocol_turn_options(render_options(120)).await;
            let receipt = match command {
                Ok(()) => None,
                Err(lash_core::SessionError::SessionCommandPending(receipt)) => Some(receipt),
                Err(error) => panic!("options command: {error}"),
            };
            if let Some(receipt) = receipt {
                let handler = double
                    .open_handler(lash_core::AdmittedScope::queue_drain(
                        &session_id,
                        "render-options-command",
                    ))
                    .await
                    .expect("open command drive handler");
                runtime
                    .drive_next_root(
                        "render-options-command",
                        lash_core::facade_support::TurnOptions::new(
                            tokio_util::sync::CancellationToken::new(),
                            handler.scoped(),
                        ),
                    )
                    .await
                    .expect("drive render options command");
                handler.close().await.expect("close command drive handler");
                assert!(matches!(
                    runtime
                        .settle_session_command(receipt)
                        .await
                        .expect("read settled render options command"),
                    lash_core::runtime::SessionCommandSettlement::Durable(_)
                ));
            }
            renderer.mode.store(1, Ordering::SeqCst);
            drive(&mut runtime, &double, &session_id, "second").await;
            assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
            assert_eq!(renderer.second.load(Ordering::SeqCst), 1);
            assert_eq!(renderer.second_limit.load(Ordering::SeqCst), 120);
            assert_eq!(attachments.puts.load(Ordering::SeqCst), 2);
            {
                let requests = script.requests.lock().expect("requests");
                let later = requests.get(2).expect("next turn request");
                assert_eq!(
                    stable_bytes(
                        &later.messages
                            [..history_prefix(requests.get(1).expect("prior request")).len()]
                    ),
                    prefix
                );
                assert!(
                    format!("{:?}", requests.get(3).expect("second post-tool request"))
                        .contains("[output cut:")
                );
            }
            drive_with_run_spec(&mut runtime, &double, &store, &session_id).await;
            assert_eq!(renderer.second.load(Ordering::SeqCst), 2);
            assert_eq!(renderer.second_limit.load(Ordering::SeqCst), 100);
            assert_eq!(attachments.puts.load(Ordering::SeqCst), 3);
            {
                let requests = script.requests.lock().expect("requests");
                let run_spec_request = requests.get(4).expect("run spec prompt");
                let length = history_prefix(requests.get(1).expect("prior request")).len();
                assert_eq!(stable_bytes(&run_spec_request.messages[..length]), prefix);
            }
            Box::pin(runtime.park()).await.expect("park runtime");
            let state = lash_core::store::load_session_window_state(
                &store,
                lash_core::store::WindowSelector::Current,
            )
            .await
            .expect("reload")
            .expect("persisted state")
            .state;
            let mut runtime = open_runtime(
                &backend,
                store,
                Arc::clone(&script),
                state,
                Arc::clone(&renderer),
                Arc::clone(&attachments),
                Arc::clone(&tool),
            )
            .await;
            drive(&mut runtime, &double, &session_id, "reopened").await;
            assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
            assert_eq!(renderer.second.load(Ordering::SeqCst), 2);
            assert_eq!(attachments.puts.load(Ordering::SeqCst), 3);
            assert_eq!(attachments.gets.load(Ordering::SeqCst), 0);
            let requests = script.requests.lock().expect("requests");
            let reopened = requests.get(6).expect("reopened request");
            let length = history_prefix(requests.get(1).expect("prior request")).len();
            assert_eq!(stable_bytes(&reopened.messages[..length]), prefix);
        });
}
