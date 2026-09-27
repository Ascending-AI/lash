#![expect(
    clippy::expect_used,
    reason = "integration fixture setup and assertions fail the law immediately"
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
    CommitBudget, LlmOutputPart, LlmResponse, ModelSpec, QueuedWorkBatchingConfig,
    RuntimePersistence, RuntimeSessionState, SessionPolicy, SessionRelation,
    SessionStoreCreateRequest, TurnBudget, TurnInput,
};
use lash_protocol_rlm::{
    CodeRenderer, CodeRendererSlot, InstructionBound, MemoryBound, RlmChannel,
    RlmProtocolPluginConfig, RlmProtocolPluginFactory,
};
use lash_sansio::llm::types::{LlmContentBlock, LlmRequest};
use lash_sansio::{SessionId, TurnId};

struct CountingRenderer {
    id: &'static str,
    prints: Arc<AtomicUsize>,
}

impl CodeRenderer for CountingRenderer {
    fn id(&self) -> &str {
        self.id
    }

    fn print(
        &self,
        value: &lashlang::Value,
        params: &lash_render::RenderParams,
    ) -> lash_render::Rendered<String> {
        self.prints.fetch_add(1, Ordering::SeqCst);
        lash_render::render(value, params)
    }
}

struct SwitchingRenderer {
    mode: AtomicUsize,
    first: AtomicUsize,
    second: AtomicUsize,
}

impl CodeRenderer for SwitchingRenderer {
    fn id(&self) -> &str {
        if self.mode.load(Ordering::SeqCst) == 0 {
            "law.first"
        } else {
            "law.second"
        }
    }

    fn print(
        &self,
        value: &lashlang::Value,
        params: &lash_render::RenderParams,
    ) -> lash_render::Rendered<String> {
        if self.mode.load(Ordering::SeqCst) == 0 {
            self.first.fetch_add(1, Ordering::SeqCst);
        } else {
            self.second.fetch_add(1, Ordering::SeqCst);
        }
        lash_render::render(value, params)
    }
}

struct Script {
    responses: Vec<String>,
    calls: AtomicUsize,
    requests: Mutex<Vec<LlmRequest>>,
}

fn policy() -> SessionPolicy {
    SessionPolicy {
        provider_id: "rlm-render-law".into(),
        model: ModelSpec::builder("rlm-render-law-model")
            .context_window_tokens(100_000)
            .build()
            .expect("model spec"),
        ..SessionPolicy::new(TurnBudget::Unbounded)
    }
}

fn provider(script: Arc<Script>) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("rlm-render-law")
        .complete(move |request| {
            let script = Arc::clone(&script);
            async move {
                let call = script.calls.fetch_add(1, Ordering::SeqCst);
                script.requests.lock().expect("request lock").push(request);
                let text = script.responses.get(call).cloned().unwrap_or_default();
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

async fn open_runtime(
    backend: &lash_core::Backend,
    store: Arc<dyn RuntimePersistence>,
    script: Arc<Script>,
    state: RuntimeSessionState,
    renderer: Arc<dyn CodeRenderer>,
    max_chars: usize,
) -> LashRuntime {
    let mut config = RlmProtocolPluginConfig::builder()
        .channel(RlmChannel::Cell)
        .instruction_limit(InstructionBound::instructions(1_000_000))
        .memory_limit(MemoryBound::mebibytes(64))
        .build();
    config.code_renderer = CodeRendererSlot(renderer);
    config.render.print.max_chars = Some(max_chars);
    let factories: Vec<Arc<dyn PluginFactory>> = vec![Arc::new(
        RlmProtocolPluginFactory::new(config, backend).with_process_lifecycle(false),
    )];
    let host = PluginHost::new(factories);
    let plugins = if let Some(snapshot) = state.plugin_state() {
        host.rematerialize_session(
            &state.session_id,
            snapshot,
            RecordedSessionConfig::new(state.protocol_turn_options.clone()),
        )
        .expect("rematerialize RLM plugin")
    } else {
        host.build_session(&state.session_id)
            .expect("build RLM plugin")
    };
    let mut host_config = RuntimeHostConfig::new(
        backend.clone(),
        CommitBudget::bounded(8 * 1024 * 1024, 1024),
        QueuedWorkBatchingConfig::new(1),
    );
    host_config.providers.provider_resolver =
        Arc::new(SingleProviderResolver::new(provider(script)));
    let runtime_host = EmbeddedRuntimeHost::new(host_config);
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
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
) {
    let mut options = lash_core::ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
        render: Some(lash_rlm_types::RlmRenderPatch {
            print: lash_render::RenderParamsPatch {
                max_chars: Some(2),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    })
    .expect("run spec render options");
    options.payload["channel"] = serde_json::json!("cell");
    store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::next_turn(),
                TurnInput::text("run-spec"),
            )
            .with_source_key("rlm-render-law-run-spec")
            .with_run_spec(lash_core::RunSpec::overrides(lash_core::RunOverrides {
                protocol_turn_options: Some(options),
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
            request: lash_core::engine::DriveRequestId::new("rlm-render-law-run-spec"),
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
        .expect("history breakpoint");
    request.messages[..=last].to_vec()
}

fn stable_history_bytes(messages: &[lash_sansio::llm::types::LlmMessage]) -> Vec<u8> {
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

#[test]
fn stored_prints_keep_the_history_cache_prefix_across_renderer_change_and_reopen() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async {
            let dir = tempfile::tempdir().expect("SQLite directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open(dir.path())
                .await
                .expect("SQLite stores");
            let double = lash_restate_test::backend_with(
                0x3931_0001,
                lash_restate_test::ServerConfig::default(),
                move |_| Arc::new(stores),
            )
            .await
            .expect("Restate double");
            let backend = double.lash_backend();
            let session_id = SessionId::from("rlm-render-cache-law");
            let base = backend
                .session_store_factory()
                .create_store(&SessionStoreCreateRequest {
                    owning_process_id: None,
                    pending_observer_intents: Vec::new(),
                    session_id: session_id.clone(),
                    relation: SessionRelation::Root,
                    policy: policy(),
                })
                .await
                .expect("create session store");
            let first_code = "let saved = \"live value\"; print(\"ok\"); print(\"abcdefgh\");";
            let next_code = "print(history[1].output[1]);";
            let script = Arc::new(Script {
                responses: vec![
                    format!("<typescript>\n{first_code}\n</typescript>"),
                    "first done".into(),
                    format!("<typescript>\n{next_code}\n</typescript>"),
                    "second done".into(),
                    "<typescript>\nprint(\"run spec\");\n</typescript>".into(),
                    "run spec done".into(),
                    "reopened done".into(),
                ],
                calls: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
            });
            let renderer = Arc::new(SwitchingRenderer {
                mode: AtomicUsize::new(0),
                first: AtomicUsize::new(0),
                second: AtomicUsize::new(0),
            });
            let mut created_options =
                lash_core::ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
                    render: Some(lash_rlm_types::RlmRenderPatch {
                        print: lash_render::RenderParamsPatch {
                            max_chars: Some(5),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .expect("create render options");
            created_options.payload["channel"] = serde_json::json!("cell");
            let mut runtime = open_runtime(
                &backend,
                base.clone(),
                Arc::clone(&script),
                RuntimeSessionState {
                    session_id: session_id.clone(),
                    protocol_turn_options: created_options,
                    ..RuntimeSessionState::new(policy())
                },
                renderer.clone(),
                9,
            )
            .await;
            drive(&mut runtime, &double, &session_id, "first").await;
            assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
            assert_eq!(script.calls.load(Ordering::SeqCst), 2);
            let (prefix, prefix_bytes) = {
                let first_requests = script.requests.lock().expect("requests");
                let first_history = first_requests.get(1).expect("request after first cell");
                let prefix = history_prefix(first_history);
                let prefix_bytes = stable_history_bytes(&prefix);
                let breakpoint = prefix.len() - 1;
                let preview_tail = serde_json::to_string(&first_history.messages[breakpoint + 1..])
                    .expect("preview tail JSON");
                assert!(preview_tail.contains("saved"), "{preview_tail}");
                assert!(
                    String::from_utf8_lossy(&prefix_bytes).contains("within 5"),
                    "first cut is recorded in the stable history"
                );
                (prefix, prefix_bytes)
            };
            let mut replacement =
                lash_core::ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
                        render: Some(lash_rlm_types::RlmRenderPatch {
                            print: lash_render::RenderParamsPatch {
                                max_chars: Some(3),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                .expect("replacement render options");
            replacement.payload["channel"] = serde_json::json!("cell");
            let command = runtime.set_protocol_turn_options(replacement).await;
            let receipt = match command {
                Ok(()) => None,
                Err(lash_core::SessionError::SessionCommandPending(receipt)) => Some(receipt),
                Err(error) => panic!("replace protocol options command: {error}"),
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
            {
                let requests = script.requests.lock().expect("requests");
                let after_change = requests.get(2).expect("request after renderer change");
                assert_eq!(
                    stable_history_bytes(&after_change.messages[..prefix.len()]),
                    prefix_bytes
                );
                let after_new_print = requests.get(3).expect("request after second cell");
                let latest_observation = after_new_print
                    .messages
                    .iter()
                    .flat_map(|message| message.blocks.iter())
                    .filter_map(|block| match block {
                        LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                        _ => None,
                    })
                    .find(|text| text.contains("history[4].output[0]:"))
                    .expect("second cell observation");
                assert!(
                    latest_observation.contains(
                        "history[4].output[0]:\n[cut: 8 chars rendered within 3; chars 1; narrow with history[4].output[0].<path>]\nabc"
                    ),
                    "{latest_observation}"
                );
            }
            drive_with_run_spec(&mut runtime, &double, &base, &session_id).await;
            assert_eq!(renderer.second.load(Ordering::SeqCst), 2);
            {
                let requests = script.requests.lock().expect("requests");
                let run_spec_request = requests.get(4).expect("run spec request");
                assert_eq!(
                    stable_history_bytes(&run_spec_request.messages[..prefix.len()]),
                    prefix_bytes
                );
                assert!(
                    format!("{:?}", requests.get(5).expect("run spec observation"))
                        .contains("within 2")
                );
            }
            Box::pin(runtime.park())
                .await
                .expect("park second runtime");
            let state = lash_core::store::load_persisted_session_state(base.as_ref())
                .await
                .expect("load second state")
                .expect("persisted second state");
            let third_calls = Arc::new(AtomicUsize::new(0));
            let third_renderer: Arc<dyn CodeRenderer> = Arc::new(CountingRenderer {
                id: "law.third",
                prints: Arc::clone(&third_calls),
            });
            let mut runtime = open_runtime(
                &backend,
                base,
                Arc::clone(&script),
                state,
                third_renderer,
                2,
            )
            .await;
            drive(&mut runtime, &double, &session_id, "reopened").await;
            let requests = script.requests.lock().expect("requests");
            let after_reopen = requests.get(6).expect("request after reopen");
            assert_eq!(
                stable_history_bytes(&after_reopen.messages[..prefix.len()]),
                prefix_bytes
            );
            assert_eq!(third_calls.load(Ordering::SeqCst), 0);
        });
}
