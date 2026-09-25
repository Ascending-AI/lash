// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::plugin::AssistantStreamTransform;

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
        parts: lash_core::session_model::shared_parts(vec![Part::reasoning(
            "foreign-replay-part".to_string(),
            "portable summary".to_string(),
            Some(lash_core::llm::types::ProviderReasoningReplay {
                signature: Some("foreign-request-signature".to_string()),
                origin: Some(lash_core::ProviderRouteIdentity::for_endpoint(
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
    lash_trace::parse_jsonl_records::<lash_trace::TraceRecord>(
        &std::fs::read_to_string(path).expect("read caller-shaped trace"),
    )
    .expect("trace rows")
    .into_iter()
    .map(|record| record.event)
    .collect()
}

fn assert_drop_survived(turn: &AssembledTurn, events: &[lash_trace::TraceEvent]) {
    assert_eq!(turn.llm_calls.len(), 1);
    assert_eq!(turn.llm_calls[0].replay_drops.len(), 1);
    assert_eq!(
        turn.llm_calls[0].replay_drops[0].reason,
        lash_core::ProviderReplayDropReason::ForeignRoute
    );
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::ProviderReplayDropped { event }
            if event.replay_kind == lash_trace::TraceProviderReplayKind::Reasoning
                && event.reason == lash_trace::TraceProviderReplayDropReason::ForeignRoute
    )));
}

async fn runtime_with_foreign_replay(
    backend: &lash_core::Backend,
    provider: TestProvider,
    plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    trace_path: &std::path::Path,
) -> LashRuntime {
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        plugins,
        Arc::new(EmptyTools),
        provider,
        test_host_config_with_trace_path(backend, trace_path.to_path_buf()),
    )
    .await;
    append_message(&mut runtime.state, foreign_replay_message());
    runtime
}

async fn run(
    backend: &lash_core::Backend,
    runtime: &mut LashRuntime,
    token: CancellationToken,
    turn_id: &TurnId,
) -> AssembledTurn {
    runtime
        .run_turn_assembled(
            TurnInput::text("continue"),
            token,
            backend_turn_scope(backend, &SessionId::from("root"), turn_id),
        )
        .await
        .expect("real runtime driver returns an assembled turn")
}

#[tokio::test]
async fn caller_shaped_completion_preserves_drop_sideband_without_provider_trace() {
    let backend = memory_backend().await;
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
    let mut runtime =
        runtime_with_foreign_replay(&backend, provider, Vec::new(), &trace_path).await;

    let turn = run(
        &backend,
        &mut runtime,
        CancellationToken::new(),
        &TurnId::from("replay-completion"),
    )
    .await;
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
    let backend = memory_backend().await;
    let trace_path = unique_trace_path("replay-sideband-failure");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            assert!(request.provider_trace.is_none());
            Err(LlmTransportError::new("original provider failure")
                .with_kind(lash_core::ProviderFailureKind::Validation)
                .with_code(FailureCode::provider("original_provider_code")))
        })
        .build();
    let mut runtime =
        runtime_with_foreign_replay(&backend, provider, Vec::new(), &trace_path).await;

    let turn = run(
        &backend,
        &mut runtime,
        CancellationToken::new(),
        &TurnId::from("replay-failure"),
    )
    .await;
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
                && error.code_namespace.as_deref() == Some("provider")
    )));
}

#[tokio::test]
async fn caller_shaped_protocol_abort_rejects_foreign_stream_and_emits_drop() {
    let backend = memory_backend().await;
    let trace_path = unique_trace_path("replay-sideband-protocol-abort");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            assert!(request.provider_trace.is_none());
            let events = request.stream_events.expect("streaming driver sender");
            events.send(LlmStreamEvent::Part(LlmOutputPart::Reasoning {
                text: "foreign streamed summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some("foreign-stream-signature".to_string()),
                    origin: Some(lash_core::ProviderRouteIdentity::for_endpoint(
                        "foreign-provider",
                        "https://foreign.example/v1",
                        "foreign-model",
                    )),
                    ..Default::default()
                }),
            }));
            events.send(LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "complete block".to_string(),
            });
            std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
        })
        .build();
    let abort_plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(StaticPluginFactory::new(
            "abort-first-chunk",
            lash_core::facade_support::PluginSpec::new().with_assistant_stream(Arc::new(
                |context| {
                    Box::pin(async move {
                        Ok(AssistantStreamTransform {
                            chunk: context.chunk,
                            abort_stream: true,
                            ..Default::default()
                        })
                    })
                },
            )),
        ));
    let mut runtime =
        runtime_with_foreign_replay(&backend, provider, vec![abort_plugin], &trace_path).await;

    let turn = run(
        &backend,
        &mut runtime,
        CancellationToken::new(),
        &TurnId::from("replay-protocol-abort"),
    )
    .await;
    let events = trace_events(&trace_path);
    assert_drop_survived(&turn, &events);
    assert!(turn.errors.iter().any(|error| {
        error.code == Some(lash_core::TurnFailureCode::ProviderReplayOriginConflict.into())
            && !error.message.contains("foreign-stream-signature")
    }));
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::LlmCallFailed { error, .. }
            if error.code.as_deref() == Some("provider_replay_origin_conflict")
                && error.code_namespace.as_deref() == Some("lash")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, lash_trace::TraceEvent::LlmCallCompleted { .. }))
    );
}

#[tokio::test]
async fn caller_shaped_cancellation_preserves_drop_sideband_without_provider_trace() {
    let backend = memory_backend().await;
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
                    .send(LlmStreamEvent::Delta {
                        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                        text: "partial output".to_string(),
                    });
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                provider_started.notify_one();
                std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
            }
        })
        .build();
    let mut runtime =
        runtime_with_foreign_replay(&backend, provider, Vec::new(), &trace_path).await;
    let cancellation = CancellationToken::new();
    let cancel_after_start = cancellation.clone();
    let canceller = lash_core::task::spawn(async move {
        started.notified().await;
        cancel_after_start.cancel();
    });

    let turn = run(
        &backend,
        &mut runtime,
        cancellation,
        &TurnId::from("replay-cancellation"),
    )
    .await;
    canceller.await.expect("canceller task");
    let events = trace_events(&trace_path);
    assert_drop_survived(&turn, &events);
    assert_eq!(turn.llm_calls[0].attempts.len(), 1);
    assert_eq!(
        turn.llm_calls[0].attempts[0].outcome,
        lash_core::AttemptOutcome::Aborted
    );
    assert_eq!(
        turn.llm_calls[0].attempts[0].protocol_position,
        lash_core::ProtocolPosition::OutputStarted
    );
    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::LlmCallFailed { error, .. }
            if error.code.as_deref() == Some("cancelled")
                && error.code_namespace.as_deref() == Some("lash")
    )));
}

#[tokio::test]
async fn confirm2_protocol_abort_conflict_retains_a_racing_provider_failure() {
    let backend = memory_backend().await;
    let trace_path = unique_trace_path("confirm2-abort-conflict-provider-failure");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            let events = request.stream_events.expect("streaming driver sender");
            events.send(LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "abort now".to_string(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Err(LlmTransportError::new("confirm2 original provider failure")
                .with_kind(lash_core::ProviderFailureKind::Stream)
                .with_code(FailureCode::provider("confirm2_original_code"))
                .with_http_status(502)
                .with_raw("confirm2 original raw provider evidence")
                .with_partial_response(LlmResponse {
                    parts: vec![LlmOutputPart::Reasoning {
                        text: "partial summary".to_string(),
                        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                            signature: Some("foreign-partial-secret".to_string()),
                            origin: Some(lash_core::ProviderRouteIdentity::for_endpoint(
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
    let abort_plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(StaticPluginFactory::new(
            "abort-before-provider-failure",
            lash_core::facade_support::PluginSpec::new().with_assistant_stream(Arc::new(
                |context| {
                    Box::pin(async move {
                        Ok(AssistantStreamTransform {
                            chunk: context.chunk,
                            abort_stream: true,
                            ..Default::default()
                        })
                    })
                },
            )),
        ));
    let mut runtime =
        runtime_with_foreign_replay(&backend, provider, vec![abort_plugin], &trace_path).await;

    let turn = run(
        &backend,
        &mut runtime,
        CancellationToken::new(),
        &TurnId::from("confirm2-abort-conflict-provider-failure"),
    )
    .await;
    assert!(turn.errors.iter().any(|error| {
        error.code == Some(lash_core::TurnFailureCode::ProviderReplayOriginConflict.into())
            && error.message.contains("confirm2 original provider failure")
    }));
    let events = trace_events(&trace_path);
    assert_drop_survived(&turn, &events);
    let issue = turn
        .errors
        .iter()
        .find(|error| {
            error.code == Some(lash_core::TurnFailureCode::ProviderReplayOriginConflict.into())
        })
        .expect("typed replay-origin conflict issue");
    assert_eq!(
        issue.provider_failure_kind,
        Some(lash_core::ProviderFailureKind::Validation)
    );
    assert_eq!(
        issue.raw.as_deref(),
        Some("confirm2 original raw provider evidence")
    );
    let original = turn.llm_calls[0].attempts[0]
        .error
        .as_ref()
        .expect("original provider failure evidence remains attached");
    assert_eq!(
        original.class,
        lash_core::ProviderFailureKind::Stream.code()
    );
    assert_eq!(
        original.code.as_ref().map(|code| code.namespaced()),
        Some("provider:confirm2_original_code".to_string())
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
                && error.code_namespace.as_deref() == Some("lash")
    )));
}

#[tokio::test]
async fn protocol_abort_commits_a_complete_cell_despite_a_conflict_free_tail_failure() {
    let backend = memory_backend().await;
    let trace_path = unique_trace_path("abort-tail-provider-failure");
    let provider = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            request
                .stream_events
                .expect("streaming driver sender")
                .send(LlmStreamEvent::Delta {
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: "complete cell".to_string(),
                });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Err(LlmTransportError::new("tail transport failure")
                .with_kind(lash_core::ProviderFailureKind::Stream)
                .with_code(FailureCode::provider("tail_transport_failure"))
                .with_http_status(502)
                .with_raw("tail raw evidence"))
        })
        .build();
    let abort_plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(StaticPluginFactory::new(
            "abort-complete-cell",
            lash_core::facade_support::PluginSpec::new().with_assistant_stream(Arc::new(
                |context| {
                    Box::pin(async move {
                        Ok(AssistantStreamTransform {
                            chunk: context.chunk,
                            abort_stream: true,
                            ..Default::default()
                        })
                    })
                },
            )),
        ));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![abort_plugin],
        Arc::new(EmptyTools),
        provider,
        test_host_config_with_trace_path(&backend, trace_path.clone()),
    )
    .await;

    let turn = run(
        &backend,
        &mut runtime,
        CancellationToken::new(),
        &TurnId::from("abort-tail-provider-failure"),
    )
    .await;
    assert!(turn.errors.is_empty(), "complete cell remains accepted");
    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(turn.llm_calls.len(), 1);
    let original = turn.llm_calls[0].attempts[0]
        .error
        .as_ref()
        .expect("tail provider failure remains in the sealed call record");
    assert_eq!(
        original.class,
        lash_core::ProviderFailureKind::Stream.code()
    );
    assert_eq!(
        original.code.as_ref().map(|code| code.namespaced()),
        Some("provider:tail_transport_failure".to_string())
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
