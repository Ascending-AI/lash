use super::*;
use crate::plugin::AssistantStreamTransform;

fn unique_trace_path(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "lash-{prefix}-{}-{}.jsonl",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

fn foreign_replay_message() -> Message {
    Message {
        id: "foreign-replay-message".to_string(),
        role: MessageRole::Assistant,
        parts: crate::session_model::shared_parts(vec![Part::reasoning(
            "foreign-replay-part".to_string(),
            "portable summary".to_string(),
            Some(crate::llm::types::ProviderReasoningReplay {
                signature: Some("foreign-request-signature".to_string()),
                origin: Some(crate::ProviderRouteIdentity::for_endpoint(
                    "foreign-provider",
                    "https://foreign.example/v1",
                    "foreign-model",
                )),
                ..Default::default()
            }),
        )]),
        origin: None,
    }
}

fn trace_events(path: &std::path::Path) -> Vec<lash_trace::TraceEvent> {
    std::fs::read_to_string(path)
        .expect("read caller-shaped trace")
        .lines()
        .map(|line| serde_json::from_str::<lash_trace::TraceRecord>(line).expect("trace row"))
        .map(|record| record.event)
        .collect()
}

fn assert_drop_survived(turn: &AssembledTurn, events: &[lash_trace::TraceEvent]) {
    assert_eq!(turn.llm_calls.len(), 1);
    assert_eq!(turn.llm_calls[0].replay_drops.len(), 1);
    assert_eq!(
        turn.llm_calls[0].replay_drops[0].reason,
        crate::ProviderReplayDropReason::ForeignRoute
    );
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::ProviderReplayDropped { event }
            if event.replay_kind == lash_trace::TraceProviderReplayKind::Reasoning
                && event.reason == lash_trace::TraceProviderReplayDropReason::ForeignRoute
    )));
}

async fn runtime_with_foreign_replay(
    provider: TestProvider,
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    trace_path: &std::path::Path,
) -> LashRuntime {
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        plugins,
        Arc::new(EmptyTools),
        provider,
        test_host_config_with_trace_path(trace_path.to_path_buf()),
    )
    .await;
    append_message(&mut runtime.state, foreign_replay_message());
    runtime
}

async fn run(runtime: &mut LashRuntime, token: CancellationToken, turn_id: &str) -> AssembledTurn {
    runtime
        .run_turn_assembled(
            TurnInput::text("continue"),
            token,
            named_turn_scope("root", turn_id),
        )
        .await
        .expect("real runtime driver returns an assembled turn")
}

#[tokio::test]
async fn caller_shaped_completion_preserves_drop_sideband_without_provider_trace() {
    let trace_path = unique_trace_path("replay-sideband-completion");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            assert!(request.provider_trace.is_none());
            assert!(!format!("{:?}", request.messages).contains("foreign-request-signature"));
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                ..Default::default()
            })
        })
        .build();
    let mut runtime = runtime_with_foreign_replay(provider, Vec::new(), &trace_path).await;

    let turn = run(&mut runtime, CancellationToken::new(), "replay-completion").await;
    let events = trace_events(&trace_path);
    assert_drop_survived(&turn, &events);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, lash_trace::TraceEvent::LlmCallCompleted { .. }))
    );
}

#[tokio::test]
async fn caller_shaped_failure_preserves_drop_sideband_and_original_error() {
    let trace_path = unique_trace_path("replay-sideband-failure");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            assert!(request.provider_trace.is_none());
            Err(LlmTransportError::new("original provider failure")
                .with_kind(crate::ProviderFailureKind::Validation)
                .with_code("original_provider_code"))
        })
        .build();
    let mut runtime = runtime_with_foreign_replay(provider, Vec::new(), &trace_path).await;

    let turn = run(&mut runtime, CancellationToken::new(), "replay-failure").await;
    let events = trace_events(&trace_path);
    assert_drop_survived(&turn, &events);
    assert!(
        turn.errors
            .iter()
            .any(|error| error.message.contains("original provider failure"))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::LlmCallFailed { error, .. }
            if error.code.as_deref() == Some("original_provider_code")
    )));
}

#[tokio::test]
async fn caller_shaped_protocol_abort_rejects_foreign_stream_and_emits_drop() {
    let trace_path = unique_trace_path("replay-sideband-protocol-abort");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            assert!(request.provider_trace.is_none());
            let events = request.stream_events.expect("streaming driver sender");
            events.send(LlmStreamEvent::Part(LlmOutputPart::Reasoning {
                text: "foreign streamed summary".to_string(),
                replay: Some(crate::llm::types::ProviderReasoningReplay {
                    signature: Some("foreign-stream-signature".to_string()),
                    origin: Some(crate::ProviderRouteIdentity::for_endpoint(
                        "foreign-provider",
                        "https://foreign.example/v1",
                        "foreign-model",
                    )),
                    ..Default::default()
                }),
            }));
            events.send(LlmStreamEvent::Delta("complete block".to_string()));
            std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
        })
        .build();
    let abort_plugin: Arc<dyn crate::PluginFactory> = Arc::new(StaticPluginFactory::new(
        "abort-first-chunk",
        crate::PluginSpec::new().with_assistant_stream(Arc::new(|context| {
            Box::pin(async move {
                Ok(AssistantStreamTransform {
                    chunk: context.chunk,
                    abort_stream: true,
                    ..Default::default()
                })
            })
        })),
    ));
    let mut runtime = runtime_with_foreign_replay(provider, vec![abort_plugin], &trace_path).await;

    let turn = run(
        &mut runtime,
        CancellationToken::new(),
        "replay-protocol-abort",
    )
    .await;
    let events = trace_events(&trace_path);
    assert_drop_survived(&turn, &events);
    assert!(turn.errors.iter().any(|error| {
        error.code.as_deref() == Some("provider_replay_origin_conflict")
            && !error.message.contains("foreign-stream-signature")
    }));
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::LlmCallFailed { error, .. }
            if error.code.as_deref() == Some("provider_replay_origin_conflict")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, lash_trace::TraceEvent::LlmCallCompleted { .. }))
    );
}

#[tokio::test]
async fn caller_shaped_cancellation_preserves_drop_sideband_without_provider_trace() {
    let trace_path = unique_trace_path("replay-sideband-cancellation");
    let started = Arc::new(tokio::sync::Notify::new());
    let provider_started = Arc::clone(&started);
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |request| {
            let provider_started = Arc::clone(&provider_started);
            async move {
                assert!(request.provider_trace.is_none());
                request
                    .stream_events
                    .expect("streaming driver sender")
                    .send(LlmStreamEvent::Delta("partial output".to_string()));
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                provider_started.notify_one();
                std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
            }
        })
        .build();
    let mut runtime = runtime_with_foreign_replay(provider, Vec::new(), &trace_path).await;
    let cancellation = CancellationToken::new();
    let cancel_after_start = cancellation.clone();
    let canceller = crate::task::spawn(async move {
        started.notified().await;
        cancel_after_start.cancel();
    });

    let turn = run(&mut runtime, cancellation, "replay-cancellation").await;
    canceller.await.expect("canceller task");
    let events = trace_events(&trace_path);
    assert_drop_survived(&turn, &events);
    assert_eq!(turn.llm_calls[0].attempts.len(), 1);
    assert_eq!(
        turn.llm_calls[0].attempts[0].outcome,
        crate::AttemptOutcome::Aborted
    );
    assert_eq!(
        turn.llm_calls[0].attempts[0].protocol_position,
        crate::ProtocolPosition::OutputStarted
    );
    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::LlmCallFailed { error, .. }
            if error.code.as_deref() == Some("cancelled")
    )));
}

#[tokio::test]
async fn confirm2_protocol_abort_conflict_retains_a_racing_provider_failure() {
    let trace_path = unique_trace_path("confirm2-abort-conflict-provider-failure");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            let events = request.stream_events.expect("streaming driver sender");
            events.send(LlmStreamEvent::Delta("abort now".to_string()));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Err(LlmTransportError::new("confirm2 original provider failure")
                .with_kind(crate::ProviderFailureKind::Stream)
                .with_code("confirm2_original_code")
                .with_status(502)
                .with_raw("confirm2 original raw provider evidence")
                .with_partial_response(LlmResponse {
                    parts: vec![LlmOutputPart::Reasoning {
                        text: "partial summary".to_string(),
                        replay: Some(crate::llm::types::ProviderReasoningReplay {
                            signature: Some("foreign-partial-secret".to_string()),
                            origin: Some(crate::ProviderRouteIdentity::for_endpoint(
                                "foreign-provider",
                                "https://foreign.example/v1",
                                "foreign-model",
                            )),
                            ..Default::default()
                        }),
                    }],
                    ..Default::default()
                }))
        })
        .build();
    let abort_plugin: Arc<dyn crate::PluginFactory> = Arc::new(StaticPluginFactory::new(
        "abort-before-provider-failure",
        crate::PluginSpec::new().with_assistant_stream(Arc::new(|context| {
            Box::pin(async move {
                Ok(AssistantStreamTransform {
                    chunk: context.chunk,
                    abort_stream: true,
                    ..Default::default()
                })
            })
        })),
    ));
    let mut runtime = runtime_with_foreign_replay(provider, vec![abort_plugin], &trace_path).await;

    let turn = run(
        &mut runtime,
        CancellationToken::new(),
        "confirm2-abort-conflict-provider-failure",
    )
    .await;
    assert!(turn.errors.iter().any(|error| {
        error.code.as_deref() == Some("provider_replay_origin_conflict")
            && error.message.contains("confirm2 original provider failure")
    }));
    let events = trace_events(&trace_path);
    assert_drop_survived(&turn, &events);
    let issue = turn
        .errors
        .iter()
        .find(|error| error.code.as_deref() == Some("provider_replay_origin_conflict"))
        .expect("typed replay-origin conflict issue");
    assert_eq!(
        issue.provider_failure_kind,
        Some(crate::ProviderFailureKind::Validation)
    );
    assert_eq!(
        issue.raw.as_deref(),
        Some("confirm2 original raw provider evidence")
    );
    let original = turn.llm_calls[0].attempts[0]
        .error
        .as_ref()
        .expect("original provider failure evidence remains attached");
    assert_eq!(original.class, crate::ProviderFailureKind::Stream.code());
    assert_eq!(
        original.provider_code.as_deref(),
        Some("confirm2_original_code")
    );
    assert_eq!(original.http_status, Some(502));
    assert!(
        original
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("confirm2 original provider failure"))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::LlmCallFailed { error, .. }
            if error.code.as_deref() == Some("provider_replay_origin_conflict")
    )));
}

#[tokio::test]
async fn protocol_abort_commits_a_complete_cell_despite_a_conflict_free_tail_failure() {
    let trace_path = unique_trace_path("abort-tail-provider-failure");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            request
                .stream_events
                .expect("streaming driver sender")
                .send(LlmStreamEvent::Delta("complete cell".to_string()));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Err(LlmTransportError::new("tail transport failure")
                .with_kind(crate::ProviderFailureKind::Stream)
                .with_code("tail_transport_failure")
                .with_status(502)
                .with_raw("tail raw evidence"))
        })
        .build();
    let abort_plugin: Arc<dyn crate::PluginFactory> = Arc::new(StaticPluginFactory::new(
        "abort-complete-cell",
        crate::PluginSpec::new().with_assistant_stream(Arc::new(|context| {
            Box::pin(async move {
                Ok(AssistantStreamTransform {
                    chunk: context.chunk,
                    abort_stream: true,
                    ..Default::default()
                })
            })
        })),
    ));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![abort_plugin],
        Arc::new(EmptyTools),
        provider,
        test_host_config_with_trace_path(trace_path.clone()),
    )
    .await;

    let turn = run(
        &mut runtime,
        CancellationToken::new(),
        "abort-tail-provider-failure",
    )
    .await;
    assert!(turn.errors.is_empty(), "complete cell remains accepted");
    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(turn.llm_calls.len(), 1);
    let original = turn.llm_calls[0].attempts[0]
        .error
        .as_ref()
        .expect("tail provider failure remains in the sealed call record");
    assert_eq!(original.class, crate::ProviderFailureKind::Stream.code());
    assert_eq!(
        original.provider_code.as_deref(),
        Some("tail_transport_failure")
    );
    assert_eq!(original.http_status, Some(502));
    assert!(
        original
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("tail transport failure"))
    );
    assert!(
        trace_events(&trace_path)
            .iter()
            .any(|event| matches!(event, lash_trace::TraceEvent::LlmCallCompleted { .. }))
    );
}
