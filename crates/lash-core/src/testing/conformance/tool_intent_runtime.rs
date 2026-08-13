use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct SignalIntentProvider {
    session_id: String,
    process_id: String,
    calls: Arc<AtomicUsize>,
}

fn signal_intent_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:conformance_signal_intent",
        "conformance_signal_intent",
        "Signal a parked process through the public intent path.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for SignalIntentProvider {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![signal_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "conformance_signal_intent").then(|| Arc::new(signal_intent_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
        panic!("the signal-intent conformance law must use AttemptContext")
    }

    fn supports_attempt_context(&self, tool_id: &crate::ToolId) -> bool {
        tool_id == signal_intent_tool().id()
    }

    async fn execute_attempt(&self, call: crate::AttemptToolCall<'_>) -> crate::ToolAttemptResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(call.context.session_id(), self.session_id);
        crate::ToolAttemptResult::done(
            crate::ToolResultDone::ok(serde_json::json!({"signalled": true})),
            crate::ToolIntents::v1(vec![crate::ToolIntent::SignalProcess(
                crate::SignalProcessIntent {
                    session_id: self.session_id.clone(),
                    process_id: self.process_id.clone(),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"tier": "durable"}),
                },
            )]),
        )
    }
}

/// Runs a literal parked-signal law through a real provider, coordinator, and
/// runtime turn over the supplied durable effect host and process registry.
#[doc(hidden)]
pub async fn public_signal_intent_wakes_parked_process(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    let session_id = format!("{prefix}-session");
    let turn_id = format!("{prefix}-turn");
    let process_id = format!("{prefix}-target");
    registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                process_id.clone(),
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::host(),
            )
            .with_extra_event_types([crate::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
            }]),
            std::slice::from_ref(&session_id),
        )
        .await
        .expect("register public signal-intent target");
    let wait_scope = effect_host
        .scoped(crate::ExecutionScope::turn(&session_id, &turn_id))
        .expect("scope durable signal wait");
    let wake_key = wait_scope
        .controller()
        .await_event_key(
            &crate::ExecutionScope::process(&process_id),
            crate::AwaitEventWaitIdentity::process_signal(&process_id, "resume", 1),
        )
        .await
        .expect("mint durable process-signal wait");
    let wait_controller = wait_scope
        .owned_controller()
        .expect("durable conformance effect scope owns its controller");
    let wait = crate::task::spawn(async move {
        wait_controller
            .await_await_event(&wake_key, tokio_util::sync::CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(SignalIntentProvider {
        session_id: session_id.clone(),
        process_id: process_id.clone(),
        calls: Arc::clone(&provider_calls),
    });
    let tool_plugin: Arc<dyn crate::facade_support::PluginFactory> =
        Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-signal-intent",
            crate::facade_support::PluginSpec::new().with_tool_provider(tools),
        ));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            move |_| {
                let model_calls = Arc::clone(&model_calls);
                async move {
                    Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                        0 => crate::LlmResponse {
                            parts: vec![crate::LlmOutputPart::ToolCall {
                                call_id: "conformance-signal-call".to_string(),
                                tool_name: "conformance_signal_intent".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..crate::LlmResponse::default()
                        },
                        1 => crate::LlmResponse {
                            full_text: "signal delivered".to_string(),
                            parts: vec![crate::LlmOutputPart::Text {
                                text: "signal delivered".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..crate::LlmResponse::default()
                        },
                        index => panic!("unexpected signal conformance model call {index}"),
                    })
                }
            }
        })
        .build();
    let mut host = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    host.control.effect_host = Arc::clone(&effect_host);
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(session_id.clone());
    let state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut runtime = Box::pin(
        crate::LashRuntime::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )
        .with_session_id(&session_id)
        .with_policy(policy)
        .with_initial_state(state)
        .with_runtime_host(host)
        .with_plugin_factories(
            crate::testing::test_standard_protocol_factories()
                .into_iter()
                .chain([tool_plugin])
                .collect(),
        )
        .with_store(Arc::new(crate::InMemorySessionStore::new()))
        .with_process_registry(Arc::clone(&registry))
        .build(),
    )
    .await
    .expect("build public signal-intent conformance runtime");
    let turn_scope = effect_host
        .scoped(crate::ExecutionScope::turn(&session_id, &turn_id))
        .expect("scope public signal-intent turn");
    let mut input = crate::TurnInput::text("signal the parked process");
    input.trace_turn_id = Some(turn_id);
    let turn = runtime
        .stream_turn(
            input,
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), turn_scope),
        )
        .await
        .expect("run public signal-intent conformance turn");
    assert!(matches!(turn.outcome, crate::TurnOutcome::Finished(_)));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), wait)
            .await
            .expect("SignalProcess intent must wake the parked durable wait")
            .expect("durable signal wait task")
            .expect("durable signal wait resolution"),
        crate::Resolution::Ok(serde_json::json!({"tier": "durable"}))
    );
    let events = registry
        .events_after(&process_id, 0)
        .await
        .expect("read durable signal target events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "signal.resume")
            .count(),
        1
    );
}
