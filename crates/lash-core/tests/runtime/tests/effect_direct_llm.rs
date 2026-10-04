use super::*;
use lash_sansio::sync::MutexExt;

const SEED: u64 = 0x5_f470;

#[tokio::test]
async fn direct_llm_completion_crosses_controller_and_records_usage_and_trace() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let recorder = RecordingEffectController::default().with_replay_by_key();
    let trace_path = unique_trace_path("direct-llm-completion");
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "raw direct answer".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 4,
                output_tokens: 6,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host = EmbeddedRuntimeHost::new({
        let mut config =
            runtime_host_config_with_effect_layer(&backend, Arc::new(recorder.clone()));
        config.tracing =
            config
                .tracing
                .clone()
                .with_trace_sink(Arc::new(lash_trace::JsonlTraceSink::new(
                    trace_path.clone(),
                )));
        config
    });
    let runtime =
        runtime_with_plugins_and_tools_and_host(Vec::new(), Arc::new(EmptyTools), transport, host)
            .await;

    let manager = runtime.runtime_session_services().expect("session manager");
    let handler = double
        .open_handler(AdmittedScope::runtime_operation(
            "test-runtime-effect-controller",
        ))
        .await
        .expect("open the operation's handler");
    let direct = manager.direct_completion_client(
        lash_core::testing::LayeredEffectHost::layer_scoped(
            handler.scoped(),
            Arc::new(recorder.clone()),
        )
        .expect("layer the operation's scoped controller"),
        None,
    );
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
            vec![LlmContentBlock::Text {
                text: Arc::from("raw prompt"),
                response_meta: None,
                cache_breakpoint: false,
            }],
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::None,
        attachment_acceptance: Default::default(),
        scope: lash_core::LlmRequestScope::new(
            "direct-llm-test",
            "direct-llm-test:frame",
            "direct-llm-test:request",
        ),
        output_spec: None,
        stream_events: None,
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    };
    let mut reused_request_id = request.clone();
    reused_request_id.messages = vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Text {
            text: Arc::from("a deliberately different prompt"),
            response_meta: None,
            cache_breakpoint: false,
        }],
    )];
    let mut missing_request_id = request.clone();
    missing_request_id.scope.request_id = "  ".to_string();
    let error = direct
        .direct_llm_completion(missing_request_id, "direct-llm-test")
        .await
        .expect_err("empty request id must be rejected before effect execution");
    assert!(error.to_string().contains("request_id must be non-empty"));
    let completion = direct
        .direct_llm_completion(request, "direct-llm-test")
        .await
        .expect("direct llm completion");
    let replayed = direct
        .direct_llm_completion(reused_request_id, "direct-llm-test")
        .await
        .expect("request-id reuse replays the first direct completion");

    assert_eq!(completion.response.full_text(), "raw direct answer");
    assert_eq!(
        replayed.response.full_text(),
        completion.response.full_text()
    );
    assert_eq!(completion.usage.output_tokens, 6);
    assert_eq!(completion.llm_call.call_id.0, "direct-effect-test");
    assert_eq!(
        recorder.count_kind(RuntimeEffectKind::Direct),
        1,
        "the same request id is the same durable effect even when request content differs"
    );
    // The recording double answers the direct effect itself: no provider was
    // dispatched, so no usage meter was admitted and nothing is accounted. Only
    // a dispatched call is spend (ADR 0127).
    drop(direct);
    handler
        .close()
        .await
        .expect("close the operation's handler");
}

/// R8: a model call really executed inside a recorded tool attempt keeps
/// the admitted Run anchor and any engine-supplied invocation identity.
#[cfg(feature = "otel-trace")]
#[tokio::test]
async fn nested_direct_completion_exports_under_its_run_scope() {
    use lash_trace::{
        TraceCandidateOutcome, TraceCause, TraceScopeFactory, TraceScopeId, TraceScopeOwner,
    };
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

    let double = kernel_double(SEED + 39, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let spans = InMemorySpanExporter::default();
    let tracer = SdkTracerProvider::builder()
        .with_simple_exporter(spans.clone())
        .build();
    let meter = SdkMeterProvider::builder().build();
    let telemetry = Arc::new(lash_trace::otel::OtelTelemetry::new(
        &tracer,
        &meter,
        Default::default(),
    ));
    let session = SessionId::fixture("nested-direct-trace");
    let run = TurnId::fixture("nested-direct-run");
    let scope_id = TraceScopeId::admission(TraceScopeOwner::Run {
        session_id: session.clone(),
        run: run.clone(),
    });
    let candidate = telemetry.propose(&scope_id, &TraceCause::Root);
    let anchor = candidate.anchor();
    candidate.settle(TraceCandidateOutcome::Selected);
    let scope = lash_trace::DurableTraceScope {
        scope: scope_id,
        cause: TraceCause::Root,
        anchor: anchor.clone(),
        started_at_ms: 1,
    };
    let mut config = test_runtime_host_config(&backend);
    config.tracing = config
        .tracing
        .clone()
        .with_scopes(telemetry.clone())
        .with_projector(telemetry);
    let transport = TestProvider::builder()
        .complete(|_| async {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "nested answer".into(),
                    response_meta: None,
                }],
                terminal_reason: LlmTerminalReason::Stop,
                ..Default::default()
            })
        })
        .build();
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        EmbeddedRuntimeHost::new(config),
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::turn(session.clone(), run.clone()))
        .await
        .expect("open Run handler");
    let scoped = handler.scoped().with_trace_scope(scope);
    let observed_invocation = scoped
        .controller()
        .attempt_observation()
        .and_then(|observation| observation.invocation_id);
    let parent = RuntimeEffectInvocation::new(
        EffectAddress::new(ExecutionScope::turn(&session, &run), "tool-attempt")
            .expect("tool attempt address"),
        RuntimeAttribution::for_turn(&session, &run, 0, 0),
        "tool-attempt",
    )
    .into_runtime_invocation();
    let direct = runtime
        .runtime_session_services()
        .expect("session services")
        .direct_completion_client(scoped, Some(run))
        .with_tool_attempt_parent_invocation(parent)
        .with_effect_attempt(Some(EffectAttempt::default()));
    let completion = direct
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("nested request"),
            "nested-trace",
        )
        .await
        .expect("nested model completion");
    assert_eq!(completion.text, "nested answer");
    drop(direct);
    handler.close().await.expect("close Run handler");
    tracer.force_flush().expect("flush completed model span");
    let exported = spans.get_finished_spans().expect("read acknowledged spans");
    let models: Vec<_> = exported
        .iter()
        .filter(|span| {
            span.attributes.iter().any(|attribute| {
                attribute.key.as_str() == "gen_ai.operation.name"
                    && attribute.value.as_str() == "chat"
            })
        })
        .collect();
    assert_eq!(
        models.len(),
        1,
        "one executed nested model call must export one span"
    );
    let anchor = anchor.context().expect("admitted OTel anchor");
    assert_eq!(
        models[0].span_context.trace_id().to_string(),
        anchor.trace_id().to_string()
    );
    assert_eq!(
        models[0].parent_span_id.to_string(),
        anchor.span_id().to_string()
    );
    let exported_invocation = models[0]
        .attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == "lash.attempt.invocation_id")
        .map(|attribute| attribute.value.as_str().into_owned());
    assert_eq!(exported_invocation, observed_invocation);
    tracer.shutdown().expect("shutdown tracer");
    meter.shutdown().expect("shutdown meter");
}
