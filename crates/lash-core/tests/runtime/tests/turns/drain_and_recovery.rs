use super::*;
use lash_core::testing::TestTurnExecution as _;

fn sid(s: &str) -> SessionId {
    SessionId::fixture(s)
}

fn tid(s: &str) -> TurnId {
    TurnId::fixture(s)
}

async fn open_admitted(
    double: &lash_restate_test::RestateTestBackend,
    admitted: AdmittedScope,
) -> lash_restate_test::OpenHandler {
    double
        .open_handler(admitted)
        .await
        .expect("open the scope's handler")
}

async fn open_turn(
    double: &lash_restate_test::RestateTestBackend,
    session: impl Into<SessionId>,
    turn: impl Into<TurnId>,
) -> lash_restate_test::OpenHandler {
    open_admitted(double, AdmittedScope::turn(session, turn)).await
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn an_in_process_shift_hands_off_after_a_bounded_number_of_runs() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let transport = TestProvider::builder()
        .kind("bounded-shift")
        .complete(|_request| async {
            Ok::<_, LlmTransportError>(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "answer".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
    );
    config.providers.models =
        lash_core::testing::standard_test_llm_profiles(transport.clone().into_handle());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        EmbeddedRuntimeHost::new(config),
        store.clone(),
    )
    .await;
    let session = sid("root");
    for index in 0..65 {
        enqueue_idle_turn_input(store.as_ref(), &session, &format!("question {index}")).await;
    }
    let request = lash_core::engine::ShiftRequest {
        session: session.clone(),
        request: lash_core::engine::ShiftRequestId::new("bounded-first"),
        intended_lane: None,
    };
    let handler = open_turn(&double, session.clone(), tid("bounded-first")).await;
    let first = Box::pin(lash_core::shift::work_session(
        &mut runtime,
        &handler.scoped(),
        &request,
    ))
    .await
    .expect("first shift runs to its bound");
    handler.close().await.expect("close the scope's handler");
    assert_eq!(first.ran.len(), lash_core::engine::MAX_RUNS_PER_SHIFT);
    assert!(matches!(
        first.stop,
        lash_core::engine::ShiftStop::Yielded { .. }
    ));
    let next = lash_core::engine::ShiftRequest {
        request: lash_core::engine::shift_continuation_request(&request),
        ..request
    };
    let handler = open_turn(&double, session.clone(), tid("bounded-next")).await;
    let last = Box::pin(lash_core::shift::work_session(
        &mut runtime,
        &handler.scoped(),
        &next,
    ))
    .await
    .expect("continuation runs the remaining run");
    handler.close().await.expect("close the scope's handler");
    assert_eq!(last.ran.len(), 1);
    assert_eq!(last.stop, lash_core::engine::ShiftStop::Idle);
}

const SEED: u64 = 0x5_f440;

// Boundary: this durable process-wake case stays in `turns.rs` because it
// asserts committed conversation history, streamed turn events, and process
// origin metadata across the full runtime, not only persistence ownership.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn durable_process_wake_drains_as_committed_event_history_and_acknowledges() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "first answer".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "acknowledged".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new("root");
    let process_caused_by = lash_core::CausalRef::SessionNode {
        session_id: sid("root"),
        node_id: "trigger:button".to_string(),
    };
    let registered = registry
        .register_process(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                lash_core::ProcessProvenance::session(target_scope.clone())
                    .with_caused_by(Some(process_caused_by.clone())),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        &registered.id,
        lash_core::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "deploy complete",
                "value": {
                    "status": "deploy complete"
                }
            }),
        ),
    )
    .await;
    let expected_wake_id = wake.wake_id();
    let expected_sequence = wake.sequence;
    let expected_text = format!(
        "Background process wake\nProcess: {}\nEvent: process.wake #{expected_sequence}\nWake input:\ndeploy complete",
        registered.id
    );

    let sink = RecordingSink::default();
    let turn_events = RecordingTurnEvents::default();
    let handler = open_turn(&double, sid("root"), tid("process-wake-turn")).await;
    runtime
        .execute_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped())
                .with_events(&sink)
                .with_turn_events(&turn_events),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let turn_event_snapshot = turn_events.snapshot();
    let queued_started = turn_event_snapshot
        .iter()
        .find(|activity| {
            matches!(
                &activity.event,
                lash_core::TurnEvent::QueuedWorkStarted { .. }
            )
        })
        .expect("queued work started event");
    let lash_core::TurnEvent::QueuedWorkStarted {
        boundary, causes, ..
    } = &queued_started.event
    else {
        panic!("expected queued work started event");
    };
    // The wake was accepted before the turn's input, so the turn lane admits
    // it first, at idle, rather than folding it into the later input's turn
    // (ADR 0101 §5).
    assert_eq!(
        *boundary,
        lash_core::testing::runtime_internals::AdmissionBoundary::Idle
    );
    assert!(causes.iter().any(|cause| {
        cause.event_type == "process.wake"
            && cause.id == expected_wake_id
            && cause.text == expected_text
            && matches!(
                &cause.origin,
                lash_core::MessageOrigin::Process {
                    process_id,
                    event_type,
                    sequence,
                    wake_id,
                    caused_by,
                } if process_id == registered.id
                    && event_type == "process.wake"
                    && *sequence == expected_sequence
                    && wake_id.as_deref() == Some(expected_wake_id.as_str())
                    && caused_by.as_ref() == Some(&process_caused_by)
            )
    }));

    assert!(
        sink.snapshot().into_iter().all(|event| {
            !matches!(
                event,
                lash_core::facade_support::SessionStreamEvent::InjectedMessagesCommitted { messages, .. }
                    if messages.iter().any(|message| message.parts.iter().any(|part| part.content() == expected_text))
            )
        }),
        "durable wake events must not be bridged as injected plugin messages"
    );
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(store.as_ref(), &sid("root"))
            .await
            .expect("queued work after commit")
            .is_empty()
    );
    let wake_history = active_conversation_messages(runtime.state())
        .into_iter()
        .find(|message| {
            message.role == lash_core::MessageRole::Event
                && message
                    .parts
                    .iter()
                    .any(|part| part.content() == expected_text)
        })
        .expect("wake history message");
    assert!(matches!(
        wake_history.origin,
        Some(lash_core::MessageOrigin::Process {
            process_id,
            event_type,
            sequence,
            wake_id,
            caused_by,
        }) if process_id == registered.id
            && event_type == "process.wake"
            && sequence == expected_sequence
            && wake_id.as_deref() == Some(expected_wake_id.as_str())
            && caused_by.as_ref() == Some(&process_caused_by)
    ));
    assert!(
        active_conversation_messages(runtime.state())
            .iter()
            .all(|message| {
                !((message.role == lash_core::MessageRole::System
                    || message.role == lash_core::MessageRole::User)
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content() == expected_text))
            }),
        "durable wake must not enter history as provider system text"
    );
}
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn session_manager_persists_child_sessions_in_separate_store() {
    let double = kernel_double(SEED + 4, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let factory = RecordingDeploymentStore::over(backend.session_store_factory());
    let backend = LayeredBackend::over(backend)
        .map_session_store_factory(|_| Arc::new(factory.clone()))
        .into_backend();
    let host = test_host_config(&backend);
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    let manager = runtime.session_state_service().expect("session state");
    let plugin_init = manager
        .session_plugin_init(&sid("root"))
        .await
        .expect("plugin init");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let handle = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("child-store")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork(
                plugin_init.clone(),
            )),
        )
        .await
        .expect("child session");

    assert_eq!(handle.session_id, "child-store");
    let stores = factory.stores();
    assert_eq!(stores.len(), 1);
    let meta = lash_core::store::SessionCommitStore::load_session_meta(
        stores[0].as_ref(),
        &SessionId::from("child-store"),
    )
    .await
    .expect("load session meta")
    .expect("session meta");
    assert_eq!(meta.session_id, "child-store");
    assert_eq!(meta.parent_session_id().map(|id| id.as_str()), Some("root"));
    let read = durable_window(stores[0].clone(), "child-store").await;
    let graph = read.window;
    let child_frame_key = lash_core::FrameKey::from_caller_material("initial-frame")
        .expect("non-empty initial frame material");
    let child_frame_node_id =
        lash_core::session_graph::frame_node_id(&meta.session_id, child_frame_key.as_str());
    assert_eq!(
        graph.nodes.first().map(|node| node.node_id.as_str()),
        Some(child_frame_node_id.as_str())
    );
    assert_eq!(
        graph
            .nodes
            .first()
            .and_then(|node| node.parent_node_id.as_deref()),
        None
    );
    assert_eq!(
        graph
            .nodes
            .iter()
            .filter(|node| matches!(
                node.payload,
                lash_core::SessionNodePayload::FrameOpen { .. }
            ))
            .count(),
        1,
        "child history must not retain the parent frame root"
    );
    let read_model = graph.read_model();
    assert!(
        read_model.messages.is_empty(),
        "an empty-start child initializes with no inherited messages"
    );
    let checkpoint = read.checkpoint.expect("checkpoint");
    assert_eq!(checkpoint.turn_state.turn_index, 0);
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn session_manager_rejects_duplicate_child_session_ids() {
    let double = kernel_double(SEED + 6, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    lifecycle
        .create_session(
            lash_core::SessionCreateRequest::root(
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork(
                plugin_init.clone(),
            )),
        )
        .await
        .expect("first child session");

    let err = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::root(
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork(
                plugin_init.clone(),
            )),
        )
        .await
        .expect_err("duplicate child session should fail");
    assert!(
        matches!(
            &err,
            lash_core::PluginError::SessionAlreadyExists { session_id } if session_id.as_str() == "child"
        ),
        "a duplicate create is the typed SessionAlreadyExists, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn turn_driver_sends_an_exact_effort_unchanged() {
    let double = kernel_double(SEED + 7, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::sync::{Arc, Mutex};

    let captured: Arc<Mutex<Option<lash_core::ReasoningSelection>>> = Arc::new(Mutex::new(None));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("capability-capture")
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                *captured.lock_recover() = Some(req.model.reasoning.clone());
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let capability = lash_core::LlmProfileCapability {
        instruction_role: Default::default(),
        native_mid_conversation_system: false,
        google_dialect: Default::default(),
        reasoning: Some(lash_core::ReasoningCapability {
            efforts: ["low", "medium", "high", "max"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..Default::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
        reasoning_retention: Default::default(),
    };
    let model = lash_core::LlmProfileMetadata::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("valid model spec")
        .with_capability(capability);

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_llm_profiles(
        &mut runtime,
        [(
            lash_core::LlmProfileKey::new("served-model"),
            lash_core::RegisteredLlmProfile::new(model, provider.clone()),
        )],
    );
    crate::runtime_support::configure_storeless(
        &mut runtime,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetLlmProfile {
            model: lash_core::LlmProfileKey::new("served-model"),
        })
        .then(lash_core::plugin::config::core::SetReasoning {
            reasoning: lash_core::ReasoningSelection::Effort("max".to_string()),
        }),
    )
    .await;

    let handler = open_turn(&double, sid("root"), tid("alias-normalize-turn")).await;
    let turn = runtime
        .execute_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    assert_eq!(turn.assistant_output.safe_text, "ok");
    let seen = captured
        .lock_recover()
        .clone()
        .expect("provider must be called");
    assert_eq!(
        seen,
        lash_core::ReasoningSelection::Effort("max".to_string()),
        "an advertised effort travels to the provider exactly as selected"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn turn_driver_rejects_unsupported_effort_before_provider_call() {
    let double = kernel_double(SEED + 8, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let called = Arc::new(AtomicBool::new(false));
    let called_for_provider = Arc::clone(&called);
    let provider = TestProvider::builder()
        .kind("capability-reject")
        .complete(move |_req| {
            let called = Arc::clone(&called_for_provider);
            async move {
                called.store(true, Ordering::SeqCst);
                Ok(LlmResponse::default())
            }
        })
        .build()
        .into_handle();

    let capability = lash_core::LlmProfileCapability {
        instruction_role: Default::default(),
        native_mid_conversation_system: false,
        google_dialect: Default::default(),
        reasoning: Some(lash_core::ReasoningCapability {
            efforts: ["low", "medium", "high"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..Default::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
        reasoning_retention: Default::default(),
    };
    let model = lash_core::LlmProfileMetadata::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("valid model spec")
        .with_capability(capability);

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_llm_profiles(
        &mut runtime,
        [(
            lash_core::LlmProfileKey::new("served-model"),
            lash_core::RegisteredLlmProfile::new(model.clone(), provider.clone()),
        )],
    );
    // A config command judges a reasoning against the model it records, so
    // this session records the unsupported effort the way creation records
    // a spec's: unjudged, leaving the turn driver's check as the gate.
    runtime.edit_resident_state_for_test(|state| {
        state.policy.model = Some(
            lash_core::LlmProfileConfig::new(lash_core::RecordedLlmProfile::mint(
                lash_core::LlmProfileKey::new("served-model"),
                model,
            ))
            .with_reasoning(lash_core::ReasoningSelection::Effort("turbo".to_string())),
        );
    });

    let handler = open_turn(&double, sid("root"), tid("unsupported-effort-turn")).await;
    let turn = runtime
        .execute_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    assert!(
        !called.load(Ordering::SeqCst),
        "an unsupported effort must be rejected before the provider is called"
    );
    let issue = turn
        .errors
        .iter()
        .find(|issue| issue.kind == lash_core::TurnFailureKind::LlmProvider)
        .expect("llm_provider issue");
    assert_eq!(
        issue.code,
        Some(lash_core::TurnFailureCode::UnsupportedEffort.into())
    );
    assert!(issue.message.contains("Unsupported effort `turbo`"));
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn session_generation_options_reach_every_provider_request() {
    let double = kernel_double(SEED + 9, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    type Captured = (lash_core::GenerationOptions, Option<u64>);
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("generation-capture")
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                captured.lock_recover().push((
                    req.generation.clone(),
                    req.model
                        .metadata()
                        .limits
                        .output_tokens
                        .default_cap()
                        .map(|cap| cap.get() as u64),
                ));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_llm_profiles(
        &mut runtime,
        [(
            lash_core::LlmProfileKey::new("served-model"),
            // A recorded model's output-cap default is model configuration,
            // not request intent: it rides the recorded limits, never the
            // generation options, and resolution layers it under them.
            lash_core::RegisteredLlmProfile::new(
                {
                    let mut metadata = lash_core::testing::test_llm_profile_metadata("mock-model");
                    metadata.limits.output_tokens =
                        lash_core::OutputTokenLimits::new(None, Some(1024))
                            .expect("valid recorded cap");
                    metadata
                },
                provider.clone(),
            ),
        )],
    );
    crate::runtime_support::configure_storeless(
        &mut runtime,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetLlmProfile {
            model: lash_core::LlmProfileKey::new("served-model"),
        }),
    )
    .await;

    let run_turn = async |runtime: &mut LashRuntime, turn_id: &TurnId| {
        let handler = open_turn(&double, sid("root"), turn_id.clone()).await;
        runtime
            .execute_turn(
                TurnInput {
                    items: vec![InputItem::Text {
                        text: "hello".to_string(),
                    }],
                    trace_turn_id: None,
                    turn_context: lash_core::TurnContext::default(),
                },
                lash_core::facade_support::TurnOptions::new(
                    CancellationToken::new(),
                    handler.scoped(),
                ),
            )
            .await
            .expect("turn");
        handler.close().await.expect("close the turn's handler");
    };

    run_turn(&mut runtime, &tid("generation-default-turn")).await;

    let requested = lash_core::GenerationOptions {
        output_token_cap: NonZeroUsize::new(64),
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(1234),
        stop_sequences: Vec::new(),
        ..Default::default()
    };
    crate::runtime_support::configure_storeless(
        &mut runtime,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(requested.clone()),
        }),
    )
    .await;
    run_turn(&mut runtime, &tid("generation-requested-turn")).await;

    let seen = captured.lock_recover().clone();
    assert_eq!(seen.len(), 2, "each turn issues one provider call");
    assert_eq!(
        seen[0],
        (lash_core::GenerationOptions::default(), Some(1_024)),
        "a session that requested nothing must not have its model's defaults echoed back as request intent"
    );
    assert_eq!(
        seen[1],
        (requested.clone(), Some(1_024)),
        "the session's generation options must reach the provider request verbatim"
    );
    assert_eq!(
        runtime.session_policy().generation,
        requested,
        "the requested options are durable session policy, not per-turn state"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn omitted_generation_options_are_reported_on_the_turn_llm_call_record() {
    let double = kernel_double(SEED + 10, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::num::NonZeroUsize;

    // The turn record says what actually reached the wire, so a host
    // asserting that nothing was dropped learns when something was: here a
    // protocol-placed cache breakpoint the adapter had no directive for.
    let dropped_sampling = lash_core::GenerationReceipt {
        output_token_cap: lash_core::GenerationOptionOutcome::Applied,
        temperature: lash_core::GenerationOptionOutcome::Applied,
        seed: lash_core::GenerationOptionOutcome::NotRequested,
        stop_sequences: lash_core::GenerationOptionOutcome::NotRequested,
        cache: lash_core::GenerationOptionOutcome::OmittedUnsupported,
        ..lash_core::GenerationReceipt::default()
    };
    let provider = TestProvider::builder()
        .kind("disposition-reporting")
        .complete(move |_req| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "ok".to_string(),
                    response_meta: None,
                }],
                generation_disposition: Some(dropped_sampling),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_llm_profiles(
        &mut runtime,
        [(
            lash_core::LlmProfileKey::new("served-model"),
            lash_core::RegisteredLlmProfile::new(
                lash_core::testing::test_llm_profile_metadata("mock-model"),
                provider.clone(),
            ),
        )],
    );
    crate::runtime_support::configure_storeless(
        &mut runtime,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetLlmProfile {
            model: lash_core::LlmProfileKey::new("served-model"),
        })
        .then(lash_core::plugin::config::core::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(
                lash_core::GenerationOptions {
                    output_token_cap: NonZeroUsize::new(128),
                    temperature: Some(
                        lash_core::NonNegativeFiniteF64::new(0.2).expect("finite temperature"),
                    ),
                    seed: Some(99),
                    stop_sequences: Vec::new(),
                    ..Default::default()
                },
            ),
        }),
    )
    .await;

    let handler = open_turn(&double, sid("root"), tid("generation-disposition-turn")).await;
    let turn = runtime
        .execute_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let attempt = turn
        .llm_calls
        .first()
        .expect("one provider call")
        .attempts
        .first()
        .expect("one attempt");
    let reported = attempt
        .generation_disposition
        .expect("the adapter reported what it sent");
    assert_eq!(reported, dropped_sampling);
    assert!(
        !reported.nothing_omitted(),
        "a host asserting nothing was dropped must be able to see the omission"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn an_output_token_cap_above_the_model_clamps_and_says_so() {
    let double = kernel_double(SEED + 11, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    // The cap is a bound, not a demand, and it is durable session policy: a
    // `update_session_config` selecting a smaller model must not leave the
    // session failing every remaining turn. It sends what the model can
    // produce, and the disposition says the number was reduced.
    type Captured = (lash_core::GenerationOptions, Option<u64>);
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("clamping-capture")
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                use lash_core::provider::{
                    GenerationEmission, GenerationWire, OutputCapWire, ThinkingSummaryWire,
                    resolve_generation_policy,
                };
                let policy = resolve_generation_policy(
                    &req,
                    "clamping-capture",
                    &GenerationWire {
                        label: "clamping-capture",
                        output_token_cap: OutputCapWire::Optional,
                        temperature: true,
                        seed: true,
                        stop_sequences: true,
                        parallel_tool_calls: true,
                        thinking_summary: ThinkingSummaryWire::NoField,
                        active_thinking_pins_sampling: false,
                    },
                )?;
                captured
                    .lock_recover()
                    .push((req.generation.clone(), policy.max_output_tokens));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    generation_disposition: Some(policy.receipt(
                        &req,
                        &GenerationEmission {
                            output_token_cap: true,
                            temperature: true,
                            ..GenerationEmission::default()
                        },
                    )),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_llm_profiles(
        &mut runtime,
        [(
            lash_core::LlmProfileKey::new("served-model"),
            lash_core::RegisteredLlmProfile::new(
                lash_core::LlmProfileMetadata::builder("small-output-model")
                    .context_window_tokens(200_000)
                    .output_token_capacity(2_048)
                    .build()
                    .expect("valid test model"),
                provider.clone(),
            ),
        )],
    );
    crate::runtime_support::configure_storeless(
        &mut runtime,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetLlmProfile {
            model: lash_core::LlmProfileKey::new("served-model"),
        })
        .then(lash_core::plugin::config::core::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(
                lash_core::GenerationOptions {
                    output_token_cap: NonZeroUsize::new(32_000),
                    temperature: Some(
                        lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature"),
                    ),
                    seed: None,
                    stop_sequences: Vec::new(),
                    ..Default::default()
                },
            ),
        }),
    )
    .await;

    let handler = open_turn(&double, sid("root"), tid("clamped-cap-turn")).await;
    let turn = runtime
        .execute_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("a cap above the model's capacity must not fail the turn");
    handler.close().await.expect("close the turn's handler");

    let seen = captured.lock_recover().clone();
    assert_eq!(
        seen.first().expect("one provider call").0.output_token_cap,
        NonZeroUsize::new(32_000),
        "the request carries the original intent to resolution"
    );
    assert_eq!(
        seen.first().expect("one provider call").1,
        Some(2048),
        "resolution sends the capacity"
    );
    assert_eq!(
        runtime.session_policy().generation.output_token_cap,
        NonZeroUsize::new(32_000),
        "clamping is per request against the current model; the session's intent is unchanged"
    );

    let reported = turn
        .llm_calls
        .first()
        .expect("one provider call")
        .attempts
        .first()
        .expect("one attempt")
        .generation_disposition
        .expect("the adapter reported what it sent");
    assert_eq!(
        reported.output_token_cap,
        lash_core::GenerationOptionOutcome::ClampedToCapacity
    );
    assert!(
        reported.nothing_omitted(),
        "a clamped cap reached the wire; it was not dropped"
    );
    assert!(
        !reported.fully_honored(),
        "a host that needs the number it asked for must be able to see the reduction"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_mid_run_generation_patch_merges_like_the_spec_overlay_does() {
    let double = kernel_double(SEED + 12, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::num::NonZeroUsize;

    // Both surfaces that set generation options speak one vocabulary. A patch
    // naming only a cap must not drop a temperature and seed the session
    // pinned — the loss `SessionSpec`'s overlay exists to prevent, one API
    // over — and replacing stays available for a host that means it.
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    let pinned = lash_core::GenerationOptions {
        output_token_cap: None,
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(42),
        stop_sequences: Vec::new(),
        ..Default::default()
    };
    crate::runtime_support::configure_storeless(
        &mut runtime,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(pinned.clone()),
        }),
    )
    .await;

    crate::runtime_support::configure_storeless(
        &mut runtime,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Merge(
                lash_core::GenerationOptions {
                    output_token_cap: NonZeroUsize::new(4_096),
                    ..Default::default()
                },
            ),
        }),
    )
    .await;
    assert_eq!(
        runtime.session_policy().generation,
        lash_core::GenerationOptions {
            output_token_cap: NonZeroUsize::new(4_096),
            temperature: pinned.temperature.clone(),
            seed: Some(42),
            stop_sequences: Vec::new(),
            ..Default::default()
        },
        "a patch that names only a cap keeps the sampling the session pinned"
    );

    crate::runtime_support::configure_storeless(
        &mut runtime,
        lash_core::ConfigTransaction::of(lash_core::plugin::config::core::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(
                lash_core::GenerationOptions::default(),
            ),
        }),
    )
    .await;
    assert_eq!(
        runtime.session_policy().generation,
        lash_core::GenerationOptions::default(),
        "an explicit replace still clears every option"
    );
}

/// FIG-4915: a fresh storeless runtime records its complete local plugin
/// admission before constructing capabilities for its first turn.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn a_fresh_storeless_runtime_materializes_its_first_turn() {
    let double = kernel_double(SEED + 15, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&provider_calls);
    let provider = TestProvider::builder()
        .kind("storeless-first-turn")
        .complete(move |_request| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Ok::<_, LlmTransportError>(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "first turn answered".to_string(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let mut core = test_host_config(&backend).core;
    core.providers.models = lash_core::testing::standard_test_llm_profiles(provider.into_handle());
    let host = lash_core::testing::test_plugin_host(Vec::new());
    assert!(!host.plugin_revisions().is_empty());
    let mut runtime = Box::pin(
        EmbeddedRuntimeBuilder::new(core, lash_core::testing::runtime_lease_owner())
            .with_session_id("root")
            .with_policy(standard_test_policy())
            .with_plugin_host(host.clone())
            .build(),
    )
    .await
    .expect("build a fresh runtime without a session store");
    let handler = open_turn(&double, sid("root"), "storeless-first-turn").await;
    let turn = runtime
        .execute_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the first storeless turn must materialize its nonempty plugin composition");
    handler
        .close()
        .await
        .expect("close the first turn's handler");

    assert_eq!(turn.assistant_output.safe_text, "first turn answered");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    let plugins = runtime.plugin_session().expect("materialized plugins");
    let admission = plugins
        .plugin_admission()
        .expect("recorded local admission");
    host.validate_plugin_admission(&admission)
        .expect("the recorded admission covers the host's complete composition");
    let composition = host.composition().expect("valid host composition");
    for (admitted, declaration) in admission.plugins().iter().zip(composition.declarations()) {
        assert_eq!(admitted.writer, declaration.format_version);
    }
    let view = lash_core::plugin::PluginNativeView::decode(
        &runtime
            .state()
            .plugin_admission_snapshot()
            .expect("recorded native view"),
        lash_core::FleetFormat::current(),
    )
    .expect("decode the first turn's plugin transition");
    assert_eq!(view.request.target, admission);
}

/// The storeless half of the empty-drain contract: with no durable store the
/// queue does not exist at all, and the drain must say so by name. Reporting
/// `ExecutionLaneBusy` or `AdmissionRefused` here would tell the host to retry or
/// abandon work that was never queued.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn an_automatic_drain_without_a_durable_queue_says_so() {
    let double = kernel_double(SEED + 13, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let mut runtime = standard_runtime_with_transport(&backend, mock_provider(Vec::new())).await;
    let handler = open_turn(&double, sid("root"), "storeless-drain").await;
    let drain = runtime
        .execute_next_queued_run(TurnOptions::new(CancellationToken::new(), handler.scoped()))
        .await
        .expect("a storeless drain still answers");
    handler.close().await.expect("close the drain's handler");
    assert!(
        matches!(
            drain,
            lash_core::facade_support::QueuedTurnDrain::Empty(
                lash_core::facade_support::EmptyQueuedDrainReason::NoDurableQueue
            )
        ),
        "a session with no durable store must report NoDurableQueue, got {drain:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(super) async fn no_queued_work_submit_defers_without_refreshing_resident_state() {
    let double = kernel_double(SEED + 14, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, mock_provider(Vec::new()))
            .await;
    let full_loads_before = store.load_session_count();
    let head_reads_before = store.load_session_head_meta_count();

    let receipt = Box::pin(runtime.submit_session_command(
        lash_core::facade_support::SessionCommand::RefreshToolCatalog {
            reason: "deferred queued lane".to_string(),
        },
        "deferred-queued-command",
    ))
    .await
    .expect("NoSessionWork leaves the durable command pending");

    assert_eq!(store.load_session_count(), full_loads_before);
    assert_eq!(store.load_session_head_meta_count(), head_reads_before);
    let pending = lash_core::store::QueuedWorkStore::list_queued_work(store.as_ref(), &sid("root"))
        .await
        .expect("inspect deferred durable command");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].batch_id, receipt.batch_id);
}

/// The shift path's test entry (FIG-3600): an idle session answers an empty
/// claim, a drain executes one run whose admission takes the head input (the
/// default drain gives each input its own run, FIG-4457), and a drain re-run
/// under the same identity replays its recorded shift instead of admitting
/// anything new; the next drain runs the next input.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn the_shift_entry_executes_one_run_per_drain_and_replays_a_repeated_drain() {
    let double = kernel_double(SEED + 9, lash_restate_test::ServerConfig::default()).await;
    let answer = |text: &str| MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    let (runtime, store) = standard_runtime_with_transport_and_double_queue_store(
        &double,
        mock_provider(vec![answer("first answer"), answer("second answer")]),
    )
    .await;
    let runtime = Arc::new(tokio::sync::Mutex::new(runtime));
    let session = sid("root");

    let idle_handler = open_turn(&double, session.clone(), tid("shift-entry-idle")).await;
    let idle = runtime
        .lock()
        .await
        .execute_next_queued_run(TurnOptions::new(
            CancellationToken::new(),
            idle_handler.scoped(),
        ))
        .await
        .expect("an idle shift answers");
    idle_handler
        .close()
        .await
        .expect("close the scope's handler");
    assert!(matches!(
        idle,
        lash_core::facade_support::QueuedTurnDrain::Empty(
            lash_core::facade_support::EmptyQueuedDrainReason::AdmissionRefused(
                lash_core::AdmissionRefusal::Empty
            )
        )
    ));

    enqueue_idle_turn_input(store.as_ref(), &session, "first question").await;
    enqueue_idle_turn_input(store.as_ref(), &session, "second question").await;

    // On the double a repeated drain replays at invocation replay, not on a
    // fresh sequential call: the first attempt drains and journals the run,
    // the crash fails that attempt, and the redrive decodes its recorded
    // verdict instead of admitting the next input.
    type ShiftDrainSlot =
        Result<lash_core::facade_support::QueuedTurnDrain<AssembledTurn>, lash_core::RuntimeError>;
    let drains: Arc<Mutex<Vec<ShiftDrainSlot>>> = Arc::new(Mutex::new(Vec::new()));
    let crashed = Arc::new(AtomicBool::new(false));
    let attempt: lash_restate_test::HandlerAttempt = {
        let runtime = Arc::clone(&runtime);
        let drains = Arc::clone(&drains);
        let crashed = Arc::clone(&crashed);
        Arc::new(move |scoped| {
            let runtime = Arc::clone(&runtime);
            let drains = Arc::clone(&drains);
            let crashed = Arc::clone(&crashed);
            Box::pin(async move {
                let drain = runtime
                    .lock()
                    .await
                    .execute_next_queued_run(TurnOptions::new(CancellationToken::new(), scoped))
                    .await;
                drains.lock_recover().push(drain);
                if !crashed.swap(true, Ordering::SeqCst) {
                    panic!("shift-entry worker crash after journaling");
                }
            })
        })
    };
    double
        .run_crashed_then_redriven(
            AdmittedScope::turn(session.clone(), tid("shift-entry-first")),
            Arc::clone(&attempt),
            attempt,
        )
        .await
        .expect("the crashed drain's invocation redrives");
    let (first, repeated) = {
        let mut drains = drains.lock_recover();
        assert_eq!(
            drains.len(),
            2,
            "the crashing attempt and its redrive each record a drain"
        );
        (
            drains.remove(0).expect("the first drain recorded"),
            drains.remove(0).expect("the replayed drain recorded"),
        )
    };
    let first = first.expect("the first drain executes a run");
    assert_eq!(first.assistant_output.safe_text, "first answer");
    let repeated = repeated.expect("the repeated drain replays");
    assert_eq!(
        repeated.assistant_output.safe_text, "first answer",
        "a repeated drain replays its recorded run, never the next input"
    );

    let handler = open_turn(&double, session.clone(), tid("shift-entry-second")).await;
    let after = runtime
        .lock()
        .await
        .execute_next_queued_run(TurnOptions::new(CancellationToken::new(), handler.scoped()))
        .await
        .expect("a later drain answers");
    handler.close().await.expect("close the scope's handler");
    assert_eq!(
        after
            .ran()
            .expect("the later drain runs the second input's own run")
            .assistant_output
            .safe_text,
        "second answer",
        "the first run's admission took the first input alone"
    );
    assert_eq!(
        lash_core::store::TurnInputStore::list_turn_input_applications(store.as_ref(), &session)
            .await
            .expect("read the applied inputs")
            .len(),
        2,
        "each input is applied exactly once"
    );
}
