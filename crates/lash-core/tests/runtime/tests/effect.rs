// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::runtime_support::effect_controller_doubles as controller_doubles;
pub(crate) use crate::runtime_support::effect_controller_doubles::*;
pub(crate) use crate::runtime_support::effect_recording_authority::*;
pub(in crate::runtime::tests) use controller_doubles::RejectingEffectController;
use controller_doubles::{StrictReplayJournal, WrongOutcomeEffectController};
use lash_core::facade_support::SessionGraphFacadeOps;
use lash_core::llm::types::{
    AttachmentSource, LlmContentBlock, LlmMessage, LlmRole, LlmToolChoice,
};
use lash_core::plugin::PluginSessionRequest;
use lash_core::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};
use lash_core::testing::TestTurnExecution as _;
use lash_sansio::sync::MutexExt;
mod fig1127;

mod fig2471;
use crate::runtime_support::effect_recording_authority as recording_authority;
mod response_settlement;
mod source_lint_support;
pub(super) use recording_authority::{
    host_with_effect_recorder, layered_effect_host, runtime_host_config_with_effect_layer,
};
use source_lint_support::unique_trace_path;

const SEED: u64 = 0x5_e100;

#[tokio::test(flavor = "multi_thread")]
async fn turn_effect_envelope_does_not_carry_checkpoint_payload() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 3,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;
    let large_marker = format!("large-turn-marker-{}", "x".repeat(16_384));

    let run_handler_0 = double
        .open_handler(AdmittedScope::turn(
            "root",
            TurnId::from("checkpoint-envelope"),
        ))
        .await
        .expect("open the Run handler");
    let turn = runtime
        .execute_turn(
            TurnInput::text(large_marker.clone()),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                scoped_test_turn(&run_handler_0, &recorder),
            ),
        )
        .await
        .expect("turn");
    run_handler_0.close().await.expect("close the Run handler");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let checkpoint_envelope = recorder
        .envelopes()
        .into_iter()
        .find(|encoded| {
            serde_json::from_str::<RuntimeEffectEnvelope>(encoded)
                .expect("decode envelope")
                .command
                .kind()
                == RuntimeEffectKind::Checkpoint
        })
        .expect("checkpoint envelope");
    assert!(!checkpoint_envelope.contains("\"turn_checkpoint\":"));
    assert!(!checkpoint_envelope.contains(&large_marker));
    assert!(!checkpoint_envelope.contains("\"messages\""));
    assert!(!checkpoint_envelope.contains("\"events\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn controller_rejection_fails_turn_explicitly() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let controller = Arc::new(RejectingEffectController::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
            &backend,
            controller.clone(),
        )),
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::turn("root", "rejecting-controller"))
        .await
        .expect("open the scope's handler");
    let turn = runtime
        .execute_turn(
            TurnInput::text("hello"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                lash_core::testing::LayeredEffectHost::layer_scoped(handler.scoped(), controller)
                    .expect("layer the handler's scope"),
            ),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the scope's handler");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(turn.errors.iter().any(|issue| {
        issue.kind == lash_core::TurnFailureKind::RuntimeEffectController
            && issue
                .code
                .as_ref()
                .is_some_and(|code| code.namespaced() == "foreign:test_controller_rejected")
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_controller_outcome_fails_turn_explicitly() {
    let double = kernel_double(SEED + 3, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let controller = Arc::new(WrongOutcomeEffectController);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
            &backend,
            controller.clone(),
        )),
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::turn("root", "wrong-outcome-controller"))
        .await
        .expect("open the scope's handler");
    let turn = runtime
        .execute_turn(
            TurnInput::text("hello"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                lash_core::testing::LayeredEffectHost::layer_scoped(handler.scoped(), controller)
                    .expect("layer the handler's scope"),
            ),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the scope's handler");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(turn.errors.iter().any(|issue| {
        issue.kind == lash_core::TurnFailureKind::RuntimeEffectController
            && issue.code.as_ref().map(|code| code.namespaced())
                == Some("lash:runtime_effect_wrong_outcome".to_string())
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_borrowed_effect_controller_uses_required_stable_turn_id() {
    let double = kernel_double(SEED + 4, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        EmbeddedRuntimeHost::new(test_runtime_host_config(&backend)),
    )
    .await;

    let run_handler_1 = double
        .open_handler(AdmittedScope::turn(
            "root",
            TurnId::from("stable-scoped-turn"),
        ))
        .await
        .expect("open the Run handler");
    let scoped_effect_controller = scoped_test_turn(&run_handler_1, &recorder);
    let turn = runtime
        .execute_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), scoped_effect_controller)
                .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");
    run_handler_1.close().await.expect("close the Run handler");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert!(recorder.records().iter().all(|record| {
        matches!(
            record.kind,
            RuntimeEffectKind::PeekAwaitEvent | RuntimeEffectKind::TransitionPlugins
        ) || record.replay_key.contains("stable-scoped-turn")
    }));
}

/// An `exec_code` effect that fails before the executor answers reaches the
/// protocol driver typed (FIG-4658 F20): the closed reason travels with the
/// message through the journal and the turn machine.
#[tokio::test(flavor = "multi_thread")]
async fn start_exec_without_code_executor_stops_as_runtime_error() {
    let double = kernel_double(SEED + 11, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let policy = SessionPolicy {
        model: Some(lash_core::testing::runtime_helpers::standard_test_llm_profile_config()),
        ..SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let plugin_session =
        lash_core::testing::test_plugin_host(vec![Arc::new(EffectControllerTestProtocolFactory {
            code_executor: None,
        })])
        .build_session(PluginSessionRequest::creation("root", Default::default()))
        .expect("plugins");
    let runtime_host = EmbeddedRuntimeHost::new(test_runtime_host_config_with_provider(
        &backend,
        mock_provider(Vec::new()).into_handle(),
    ));
    let runtime_services = RuntimeServices::new(
        plugin_session,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        policy,
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root").clone(),
            TurnId::from("exec-without-executor").clone(),
        ))
        .await
        .expect("open the scope's handler");
    let turn = runtime
        .execute_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run code".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the scope's handler");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    let received = turn
        .errors
        .iter()
        .find_map(|issue| serde_json::from_str::<serde_json::Value>(&issue.message).ok())
        .expect("the driver reports the failure it received");
    assert_eq!(
        received,
        serde_json::json!({
            "reason": "executor_unavailable",
            "message": "code execution is not available in this session",
        })
    );
}

/// A recorded execution-environment sync failure fails the turn under its
/// cause's own code (FIG-4658 F20): the plugin's refusal is classified once,
/// journaled typed, and reaches the turn's error still carrying that code
/// instead of collapsing into `reconfigure_failed`.
#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_sync_failure_fails_the_turn_under_its_causes_code() {
    let double = kernel_double(SEED + 30, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let recorder = RecordingEffectController::default();
    let policy = SessionPolicy {
        model: Some(lash_core::testing::runtime_helpers::standard_test_llm_profile_config()),
        ..SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let plugin_session =
        lash_core::testing::test_plugin_host(vec![Arc::new(PromptRefusingProtocolFactory)])
            .build_session(PluginSessionRequest::creation("root", Default::default()))
            .expect("plugins");
    let runtime_host = host_with_effect_recorder(&backend, recorder.clone());
    let runtime_services = RuntimeServices::new(
        plugin_session,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        policy,
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");

    let run_handler_2 = double
        .open_handler(AdmittedScope::turn("root", TurnId::from("sync-refused")))
        .await
        .expect("open the Run handler");
    let turn = runtime
        .execute_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run code".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                scoped_test_turn(&run_handler_2, &recorder),
            ),
        )
        .await
        .expect("turn");
    run_handler_2.close().await.expect("close the Run handler");

    assert_eq!(
        recorder.count_kind(RuntimeEffectKind::SyncExecutionEnvironment),
        1,
        "a recorded refusal is the sync's outcome, not an attempt fault"
    );
    assert_eq!(recorder.count_kind(RuntimeEffectKind::ExecCode), 0);
    let issue = turn
        .errors
        .iter()
        .find(|issue| issue.kind == lash_core::TurnFailureKind::ExecutionEnvironment)
        .unwrap_or_else(|| panic!("the turn fails on its environment: {:?}", turn.errors));
    assert_eq!(
        issue.code,
        Some(lash_core::FailureCode::from(
            &lash_core::RuntimeErrorCode::ProtocolBeforeLlmCall
        )),
    );
    assert!(issue.message.contains(PROMPT_REFUSAL), "{issue:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_completion_journals_the_owners_recorded_binding() {
    let double = kernel_double(SEED + 4655, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let recorder = RecordingEffectController::default();
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;
    let owner = runtime
        .session_policy()
        .model
        .as_ref()
        .expect("recorded profile")
        .clone();
    let manager = runtime
        .runtime_session_services()
        .expect("session services");
    let direct = manager.direct_completion_client(
        ScopedEffectController::shared(
            layered_operation_controller(&backend, Arc::new(recorder.clone())),
            lash_core::AdmittedScope::runtime_operation("recorded-direct-binding"),
        )
        .expect("operation scope"),
        None,
    );
    let request = lash_core::facade_support::DirectRequest::text("summarize");
    direct
        .direct_completion(request, "binding-law")
        .await
        .expect("completion");
    let mut forged = owner.clone();
    forged.model = lash_core::RecordedLlmProfile::mint(
        lash_core::LlmProfileKey::new("caller-profile"),
        lash_core::LlmProfileMetadata::builder("another-model")
            .context_window_tokens(4096)
            .output_token_capacity(1024)
            .max_output_tokens(512)
            .extra_body(
                serde_json::json!({"caller_route": true})
                    .as_object()
                    .expect("object")
                    .clone(),
            )
            .build()
            .expect("valid caller metadata"),
    );
    forged.reasoning = lash_core::ReasoningSelection::Effort("unsupported-caller-effort".into());
    let mut raw = lash_core::direct::build_llm_request(
        lash_core::facade_support::DirectRequest::text("raw summarize"),
        forged,
    )
    .expect("request");
    raw.scope.request_id = "raw-binding-law".into();
    direct
        .direct_llm_completion(raw, "raw-binding-law")
        .await
        .expect("raw completion");
    let requests: Vec<_> = recorder
        .envelopes()
        .into_iter()
        .map(|wire| serde_json::from_str::<RuntimeEffectEnvelope>(&wire).expect("envelope"))
        .filter_map(|envelope| match envelope.command {
            RuntimeEffectCommand::Direct { request, .. } => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.model, owner);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn in_turn_direct_completion_uses_effect_controller_without_out_of_band_commit() {
    let double = kernel_double(SEED + 13, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let recorder = RecordingEffectController::default();
    let store = double_unbound_recording_store(&double).await;
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "direct answer".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 7,
                output_tokens: 5,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host = EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
        &backend,
        Arc::new(recorder.clone()),
    ));
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        host,
        store.clone(),
    )
    .await;
    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        ScopedEffectController::shared(
            layered_operation_controller(&backend, Arc::new(recorder.clone())),
            lash_core::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        )
        .expect("valid test runtime scope"),
        Some(TurnId::from("turn-direct")),
    );
    let completion = direct
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("summarize"),
            "direct-test",
        )
        .await
        .expect("direct completion");

    assert_eq!(completion.text, "direct answer");
    assert!(recorder.records().iter().any(|record| {
        record.kind == RuntimeEffectKind::Direct && record.turn_id.as_deref() == Some("turn-direct")
    }));

    // A direct effect's usage is its own run's accounting, delivered with the
    // effect (ADR 0127). The direct path must NOT issue its own out-of-band
    // `commit_runtime_state` mid-turn: doing so races the owning turn's
    // head-revision CAS.
    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        0,
        "in-turn direct completion must not commit runtime state out-of-band"
    );
    // The recording double answers the direct effect itself: no provider was
    // dispatched, so no usage meter was admitted and nothing is accounted. Only
    // a dispatched call is spend (ADR 0127).
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_clients_from_one_turn_share_sequential_replay_ordinals() {
    let double = kernel_double(SEED + 14, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let recorder = RecordingEffectController::default();
    let response = || MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "direct answer".to_string(),
                response_meta: None,
            }],
            ..LlmResponse::default()
        }),
    };
    let host = EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
        &backend,
        Arc::new(recorder.clone()),
    ));
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![response(), response()]),
        host,
    )
    .await;
    let manager = runtime.runtime_session_services().expect("session manager");
    let first = manager.direct_completion_client(
        ScopedEffectController::shared(
            layered_operation_controller(&backend, Arc::new(recorder.clone())),
            lash_core::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        )
        .expect("valid test runtime scope"),
        Some(TurnId::from("turn-direct")),
    );
    let second = manager.direct_completion_client(
        ScopedEffectController::shared(
            layered_operation_controller(&backend, Arc::new(recorder.clone())),
            lash_core::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        )
        .expect("valid test runtime scope"),
        Some(TurnId::from("turn-direct")),
    );

    first
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("first"),
            "direct-test",
        )
        .await
        .expect("first direct completion");
    second
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("second"),
            "direct-test",
        )
        .await
        .expect("second direct completion");

    let replay_keys = recorder
        .records()
        .into_iter()
        .filter(|record| record.kind == RuntimeEffectKind::Direct)
        .map(|record| record.replay_key)
        .collect::<Vec<_>>();
    assert_eq!(replay_keys.len(), 2);
    assert!(replay_keys[0].starts_with("direct:v3:blake3:"));
    assert!(replay_keys[1].starts_with("direct:v3:blake3:"));
    assert_ne!(replay_keys[0], replay_keys[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_concurrency_requires_keys_and_releases_unkeyed_guard() {
    let double = kernel_double(SEED + 15, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let gate = Arc::new((
        tokio::sync::Notify::new(),
        tokio::sync::Notify::new(),
        std::sync::atomic::AtomicBool::new(true),
    ));
    let recorder = RecordingEffectController::default().with_direct_gate(Arc::clone(&gate));
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
            &backend,
            Arc::new(recorder.clone()),
        )),
    )
    .await;
    let manager = runtime.runtime_session_services().expect("session manager");
    let client = manager.direct_completion_client(
        ScopedEffectController::shared(
            layered_operation_controller(&backend, Arc::new(recorder)),
            lash_core::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        )
        .expect("valid test runtime scope"),
        Some(TurnId::from("turn-direct")),
    );

    let first_client = client.clone();
    let mut first = lash_core::task::spawn(async move {
        first_client
            .direct_completion(
                lash_core::facade_support::DirectRequest::text("first"),
                "direct-test",
            )
            .await
    });
    gate.0.notified().await;
    client
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("other hook"),
            "other-plugin-hook",
        )
        .await
        .expect("a distinct usage source owns an independent ordinal lane");
    let overlap = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.direct_completion(
            lash_core::facade_support::DirectRequest::text("overlap"),
            "direct-test",
        ),
    )
    .await
    .expect("overlap rejected promptly")
    .expect_err("overlapping unkeyed call must fail");
    assert!(overlap.to_string().contains("explicit replay keys"));
    gate.1.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), &mut first)
        .await
        .expect("first completion returned")
        .expect("first task")
        .expect("first completion");

    let keyed_a = client.direct_completion(
        lash_core::facade_support::DirectRequest::text("a").with_replay_key("a"),
        "direct-test",
    );
    let keyed_b = client.direct_completion(
        lash_core::facade_support::DirectRequest::text("b").with_replay_key("b"),
        "direct-test",
    );
    let (a, b) = tokio::join!(keyed_a, keyed_b);
    a.expect("first keyed call");
    b.expect("second keyed call");
    client
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("after"),
            "direct-test",
        )
        .await
        .expect("guard released after completion");
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_effect_restores_required_streaming_for_provider_execution() {
    let double = kernel_double(SEED + 16, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let saw_stream_events = Arc::new(AtomicBool::new(false));
    let captured = Arc::clone(&saw_stream_events);
    let transport = TestProvider::builder()
        .kind("stream-required")
        .requires_streaming(true)
        .complete(move |request| {
            let captured = Arc::clone(&captured);
            async move {
                captured.store(request.stream_events.is_some(), Ordering::SeqCst);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "direct answer".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        EmbeddedRuntimeHost::new(test_runtime_host_config(&backend)),
    )
    .await;

    let manager = runtime.runtime_session_services().expect("session manager");
    let handler = double
        .open_handler(lash_core::AdmittedScope::runtime_operation(
            "test-runtime-effect-controller",
        ))
        .await
        .expect("open the scope's handler");
    let direct = manager.direct_completion_client(handler.scoped(), None);
    let completion = direct
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("summarize"),
            "direct-test",
        )
        .await
        .expect("direct completion");

    drop(direct);
    handler.close().await.expect("close the scope's handler");
    assert_eq!(completion.text, "direct answer");
    assert!(saw_stream_events.load(Ordering::SeqCst));
}

#[path = "effect_direct_llm.rs"]
mod direct_llm;

#[tokio::test(flavor = "multi_thread")]
async fn direct_llm_completion_envelope_stores_attachment_refs_not_bytes() {
    let double = kernel_double(SEED + 17, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let recorder = RecordingEffectController::default();
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(test_runtime_host_config(&backend)),
    )
    .await;

    let image_bytes = vec![137, 80, 78, 71];
    let expected_attachment_id = lash_core::attachments::content_id(&image_bytes).to_string();
    let request = LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder("mock-model".to_string())
                    .context_window_tokens(128_000)
                    .capability(lash_core::LlmProfileCapability::default())
                    .extra_body(Default::default())
                    .request_defaults(Default::default())
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment {
                source: Box::new(AttachmentSource::inline(
                    lash_core::MediaType::parse("image/png").unwrap(),
                    image_bytes,
                )),
            }],
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::None,
        attachment_acceptance: Default::default(),
        scope: lash_core::LlmRequestScope::new(
            "direct-attachment-test",
            "direct-attachment-test:frame",
            "direct-attachment-test:request",
        ),
        output_spec: None,
        stream_events: None,
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    };

    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        ScopedEffectController::shared(
            layered_operation_controller(&backend, Arc::new(recorder.clone())),
            lash_core::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        )
        .expect("valid test runtime scope"),
        None,
    );
    let completion = direct
        .direct_llm_completion(request, "direct-image-test")
        .await
        .expect("direct llm completion");

    assert_eq!(completion.response.full_text(), "raw direct answer");
    let envelope = recorder
        .envelopes()
        .into_iter()
        .find(|envelope| envelope.contains("\"type\":\"direct\""))
        .expect("direct llm envelope");
    assert!(!envelope.contains("\"data\""));
    assert!(envelope.contains(&expected_attachment_id));
}

#[cfg(test)]
mod effect_driver_support;
use effect_driver_support::{
    EffectControllerTestCodeExecutor, EffectControllerTestProtocolFactory, PROMPT_REFUSAL,
    PromptRefusingProtocolFactory,
};

struct PreludeContextHooks {
    live_revision: Arc<std::sync::atomic::AtomicUsize>,
    pressure_calls: Arc<std::sync::atomic::AtomicUsize>,
    prepare_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_core::plugin::ContextPressureHook for PreludeContextHooks {
    fn id(&self) -> &'static str {
        "prelude-pressure"
    }

    async fn decide(
        &self,
        _ctx: &lash_core::plugin::ContextPressureContext<'_>,
    ) -> Result<lash_core::plugin::ContextPressureDecision, lash_core::plugin::ContextError> {
        self.pressure_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(
            if self.live_revision.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                lash_core::plugin::ContextPressureDecision::Record { nodes: Vec::new() }
            } else {
                lash_core::plugin::ContextPressureDecision::Continue
            },
        )
    }
}

#[async_trait::async_trait]
impl lash_core::plugin::TurnContextTransform for PreludeContextHooks {
    fn id(&self) -> &'static str {
        "prelude-prepare"
    }

    async fn transform(
        &self,
        _ctx: &lash_core::plugin::TurnTransformContext<'_>,
        mut input: lash_core::facade_support::PreparedContext,
    ) -> Result<lash_core::facade_support::PreparedContext, lash_core::plugin::ContextError> {
        self.prepare_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.live_revision.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            input.messages = lash_core::facade_support::MessageSequence::default();
        }
        Ok(input)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_adopts_the_recorded_prelude_despite_changed_live_preparation_and_config() {
    let double = kernel_double(SEED + 48, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let live_revision = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pressure_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let prepare_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hooks = Arc::new(PreludeContextHooks {
        live_revision: Arc::clone(&live_revision),
        pressure_calls: Arc::clone(&pressure_calls),
        prepare_calls: Arc::clone(&prepare_calls),
    });
    let factory: Arc<dyn lash_core::plugin::PluginFactory> = Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("prelude-context"),
        PluginSpec::new()
            .with_context_pressure_hook(0, hooks.clone())
            .with_turn_context_transform(0, hooks),
    ));
    let recorder = RecordingEffectController::default().with_strict_replay_by_address();
    let mut first = runtime_with_plugins_and_tools_and_host(
        vec![factory.clone()],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;
    let run = TurnId::from("recorded-prelude");
    let run_handler_3 = double
        .open_handler(AdmittedScope::turn("root", &run))
        .await
        .expect("open the Run handler");
    let original = first
        .execute_turn(
            TurnInput::text("retain this prepared context"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                scoped_test_turn(&run_handler_3, &recorder),
            ),
        )
        .await
        .expect("first execution");
    run_handler_3.close().await.expect("close the Run handler");
    let (configuration, prelude) = {
        let records = recorder.strict_replay.outcomes.lock_recover();
        let configuration = records
            .values()
            .find_map(|(_, outcome)| match outcome.as_ref().ok()? {
                RuntimeEffectOutcome::ResolveTurnConfig { resolved } => Some(resolved.clone()),
                _ => None,
            })
            .expect("recorded config");
        let prelude = records
            .values()
            .find_map(|(_, outcome)| match outcome.as_ref().ok()? {
                RuntimeEffectOutcome::SyncExecutionEnvironment { prelude, .. } => {
                    Some(prelude.clone())
                }
                _ => None,
            })
            .expect("recorded prelude");
        (configuration, prelude)
    };
    assert!(configuration.termination.treat_missing_done_as_failure);
    assert_eq!(prelude.pressure.len(), 1);
    assert!(!prelude.context.messages.is_empty());
    let calls = *recorder.llm_calls.lock_recover();
    live_revision.store(1, std::sync::atomic::Ordering::SeqCst);
    let mut host = host_with_effect_recorder(&backend, recorder.clone());
    host.core.control.termination.treat_missing_done_as_failure = false;
    let mut replay = runtime_with_plugins_and_tools_and_host(
        vec![factory],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    let run_handler_4 = double
        .open_handler(AdmittedScope::turn("root", &run))
        .await
        .expect("open the Run handler");
    let replayed = replay
        .execute_turn(
            TurnInput::text("retain this prepared context"),
            lash_core::facade_support::TurnOptions::new(
                CancellationToken::new(),
                scoped_test_turn(&run_handler_4, &recorder),
            ),
        )
        .await
        .expect("replay uses recorded context before constructing the model request");
    run_handler_4.close().await.expect("close the Run handler");
    assert_eq!(replayed.outcome, original.outcome);
    assert_eq!(
        *recorder.llm_calls.lock_recover(),
        calls,
        "the model effect executes once"
    );
    assert_eq!(pressure_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(prepare_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        recorder.count_kind(RuntimeEffectKind::SyncExecutionEnvironment),
        1
    );
}
