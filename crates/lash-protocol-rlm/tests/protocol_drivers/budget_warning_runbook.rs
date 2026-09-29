use std::sync::{Arc, Mutex};

use lash_core::facade_support::SessionStreamEvent;
use lash_core::facade_support::{
    EmbeddedRuntimeHost, LashRuntime, PersistentRuntimeServices, PluginHost, RuntimeHostConfig,
    SingleProviderResolver,
};
use lash_core::plugin::PluginFactory;
use lash_core::testing::TestTurnDrive as _;
use lash_core::testing::runtime_helpers::RecordingSink;
use lash_core::{
    CommitBudget, LlmOutputPart, LlmResponse, ModelSpec, PluginRuntimeEvent,
    QueuedWorkBatchingConfig, RuntimeSessionState, SessionNodePayload, SessionPolicy,
    SessionRelation, SessionStoreCreateRequest, TurnBudget, TurnInput,
};
use lash_protocol_rlm::{
    InstructionBound, MemoryBound, RlmChannel, RlmProtocolPluginConfig, RlmProtocolPluginFactory,
};
use lash_sansio::llm::types::{LlmRequest, LlmUsage};
use lash_sansio::{SessionId, TurnId};

#[test]
fn scripted_context_budget_warning_reaches_model_and_continue_as_carries_only_seed() {
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
                0x4042_0001,
                lash_restate_test::ServerConfig::default(),
                move |_| Arc::new(stores),
            )
            .await
            .expect("Restate double");
            let backend = double.lash_backend();
            let session_id = SessionId::from("context-budget-runbook");
            let policy = SessionPolicy {
                provider_id: "scripted-budget-provider".into(),
                model: ModelSpec::builder("scripted-budget-model")
                    .context_window_tokens(41_000)
                    .build()
                    .expect("model spec"),
                ..SessionPolicy::new(TurnBudget::Unbounded)
            };
            let store = backend
                .session_store_factory()
                .create_store(&SessionStoreCreateRequest {
                    owning_process_id: None,
                    pending_observer_intents: Vec::new(),
                    session_id: session_id.clone(),
                    relation: SessionRelation::Root,
                    policy: policy.clone(),
                })
                .await
                .expect("create session store");
            let mut config = RlmProtocolPluginConfig::builder()
                .channel(RlmChannel::Cell)
                .instruction_limit(InstructionBound::instructions(1_000_000))
                .memory_limit(MemoryBound::mebibytes(64))
                .build();
            config.continue_as_soft_warn_tokens = Some(100);
            let factories: Vec<Arc<dyn PluginFactory>> = vec![Arc::new(
                RlmProtocolPluginFactory::new(config, &backend).with_process_lifecycle(false),
            )];
            let plugins = PluginHost::new(factories)
                .build_session(&session_id)
                .expect("build RLM plugin");

            let requests = Arc::new(Mutex::new(Vec::<LlmRequest>::new()));
            let scripted_requests = Arc::clone(&requests);
            let provider = lash_core::testing::TestProvider::builder()
                .kind("scripted-budget-provider")
                .complete(move |request| {
                    let requests = Arc::clone(&scripted_requests);
                    async move {
                        let mut seen = requests.lock().expect("request lock");
                        let call = seen.len();
                        seen.push(request);
                        let (text, input_tokens) = match call {
                            0 => ("<typescript>\nfinish(\"pressure\");\n</typescript>", 120),
                            1 => ("<typescript>\nprint(\"warning observed\");\n</typescript>", 8),
                            2 => (
                                "<typescript>\nawait control.continue_as({ task: \"answer from the baton\", seed: { baton: \"SEED-BATON-4042\" } });\n</typescript>",
                                8,
                            ),
                            3 => ("<typescript>\nfinish(\"SEED-BATON-4042\");\n</typescript>", 8),
                            _ => panic!("unexpected scripted provider call {call}"),
                        };
                        Ok(LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: text.to_string(),
                                response_meta: None,
                            }],
                            usage: LlmUsage {
                                input_tokens,
                                ..Default::default()
                            },
                            ..Default::default()
                        })
                    }
                })
                .build()
                .into_handle();
            let mut host_config = RuntimeHostConfig::new(
                backend,
                CommitBudget::bounded(8 * 1024 * 1024, 1024),
                QueuedWorkBatchingConfig::new(1),
            );
            host_config.providers.provider_resolver =
                Arc::new(SingleProviderResolver::new(provider));
            let host = EmbeddedRuntimeHost::new(host_config);
            let services = PersistentRuntimeServices::new(
                plugins,
                store,
                Arc::clone(&host.core.durability.attachment_store),
                Arc::clone(&host.core.durability.process_env_store),
            );
            let mut runtime = LashRuntime::from_persistent_embedded_state(
                policy.clone(),
                host,
                services,
                RuntimeSessionState {
                    session_id: session_id.clone(),
                    ..RuntimeSessionState::new(policy)
                },
                lash_core::testing::runtime_lease_owner(),
            )
            .await
            .expect("open runtime");
            runtime
                .configure_protocol_on_materialize(&lash_core::PluginOptions::empty(), true)
                .expect("materialize protocol");
            let events = RecordingSink::default();
            let pressure_handler = double
                .open_handler(lash_core::AdmittedScope::turn(
                    &session_id,
                    TurnId::from("budget-pressure"),
                ))
                .await
                .expect("open turn handler");
            let pressure = runtime
                .drive_turn_frames(
                    TurnInput::text("OLD-ONLY-4042"),
                    lash_core::facade_support::TurnOptions::new(
                        tokio_util::sync::CancellationToken::new(),
                        pressure_handler.scoped(),
                    )
                    .with_events(&events),
                )
                .await
                .expect("drive pressure turn");
            pressure_handler.close().await.expect("close pressure handler");
            assert_eq!(pressure.turns.len(), 1);
            assert!(runtime.state().token_usage.total() >= 120);
            let handler = double
                .open_handler(lash_core::AdmittedScope::turn(
                    &session_id,
                    TurnId::from("budget-switch"),
                ))
                .await
                .expect("open switch handler");
            let result = runtime
                .drive_turn_frames(
                    TurnInput::text("switch now"),
                    lash_core::facade_support::TurnOptions::new(
                        tokio_util::sync::CancellationToken::new(),
                        handler.scoped(),
                    )
                    .with_events(&events),
                )
                .await
                .expect("drive RLM turn");
            handler.close().await.expect("close turn handler");

            let requests = requests.lock().expect("request lock");
            assert_eq!(requests.len(), 4, "one pressure call, two warned calls, one follow call");
            let warned_request = serde_json::to_string(&requests[1]).expect("warned request JSON");
            assert!(warned_request.contains("Past the frame switch threshold"), "{warned_request}");
            assert!(warned_request.contains("control.continue_as"), "{warned_request}");
            let follow_request = serde_json::to_string(&requests[3]).expect("follow request JSON");
            assert!(follow_request.contains("SEED-BATON-4042"), "{follow_request}");
            assert!(!follow_request.contains("OLD-ONLY-4042"), "{follow_request}");

            let warnings = events
                .snapshot()
                .into_iter()
                .filter_map(|event| match event {
                    SessionStreamEvent::PluginEvent {
                        event: PluginRuntimeEvent::Status { key, detail, .. },
                        ..
                    } if key == "rlm_context_budget_warning" => detail,
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(warnings.len(), 1, "the threshold emits one typed status");
            assert!(warnings[0].contains("warn at 100"), "{:?}", warnings);
            assert!(warnings[0].contains("120 tokens used"), "{:?}", warnings);

            assert_eq!(result.turns.len(), 2, "continue_as opens a follow-frame turn");
            let frame = runtime.state().current_frame_node_id.as_ref().expect("new frame");
            let frame_node = runtime
                .state()
                .session_graph
                .nodes
                .iter()
                .find(|node| node.node_id.as_str() == frame.as_str())
                .expect("current frame node");
            assert!(matches!(
                &frame_node.payload,
                SessionNodePayload::FrameOpen { reason, .. } if reason.as_str() == "continue_as"
            ));
            let read = runtime.state().read_model().expect("frame read model");
            let read_json = serde_json::to_string(&read.messages).expect("read model JSON");
            assert!(!read_json.contains("OLD-ONLY-4042"), "{read_json}");
            let scoped = runtime
                .state()
                .session_graph
                .read_model(Some(frame))
                .expect("new frame graph view");
            let seeds = scoped
                .active_events
                .iter()
                .filter_map(|event| match event {
                    lash_core::SessionHistoryRecord::Protocol(event) => {
                        lash_protocol_rlm::decode_rlm_protocol_event(event)
                    }
                    _ => None,
                })
                .filter_map(|event| match event {
                    lash_rlm_types::RlmProtocolEvent::RlmSeed(seed) => Some(seed),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(seeds.len(), 1);
            assert_eq!(seeds[0].globals["baton"], serde_json::json!("SEED-BATON-4042"));
        });
}
