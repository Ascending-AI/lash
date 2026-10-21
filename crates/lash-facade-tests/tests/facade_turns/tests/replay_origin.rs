//! A reasoning replay recorded on another provider route never reaches this
//! route's request: the call drops it and records the drop beside its
//! outcome, whether the call completes, fails, is aborted by the protocol or
//! is cancelled. On facade turns the session actor runs.

use super::*;
use lash::provider::LlmTransportError;
use lash_core::FailureCode;
use lash_core::facade_support::TurnOutcome;
use lash_core::plugin::AssistantStreamTransform;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

fn foreign_route() -> lash_core::ProviderRouteIdentity {
    lash_core::ProviderRouteIdentity::for_endpoint(
        "foreign-provider",
        "https://foreign.example/v1",
        "foreign-model",
    )
}

/// An assistant message whose reasoning replay was recorded on another
/// provider route.
fn foreign_replay_message() -> lash_core::PluginMessage {
    lash_core::PluginMessage {
        id: Some("foreign-replay-message".to_string()),
        role: lash_core::MessageRole::Assistant,
        origin: None,
        parts: vec![lash_core::Part::reasoning(
            "foreign-replay-part".to_string(),
            "portable summary".to_string(),
            Some(lash_core::llm::types::ProviderReasoningReplay {
                signature: Some("foreign-request-signature".to_string()),
                origin: Some(foreign_route()),
                ..Default::default()
            }),
        )],
    }
}

/// A plugin that aborts the assistant stream at its first chunk.
fn abort_first_chunk(id: &'static str) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(id),
        lash_core::facade_support::PluginSpec::new().with_assistant_stream(
            lash_core::hook_key!("assistant-stream-abort"),
            Arc::new(|context| {
                Box::pin(async move {
                    Ok(AssistantStreamTransform {
                        chunk: context.chunk,
                        abort_stream: true,
                        ..Default::default()
                    })
                })
            }),
        ),
    ))
}

/// A plugin that aborts the assistant stream at its first chunk once the
/// provider has returned: the call's provider failure races the protocol's
/// acceptance of the stream and is already in when the cell is accepted.
fn abort_first_chunk_after_provider_returned(
    id: &'static str,
    returned: Arc<tokio::sync::Notify>,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(id),
        lash_core::facade_support::PluginSpec::new().with_assistant_stream(
            lash_core::hook_key!("assistant-stream-abort"),
            Arc::new(move |context| {
                let returned = Arc::clone(&returned);
                Box::pin(async move {
                    returned.notified().await;
                    // The provider task seals the call after its future
                    // returns.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok(AssistantStreamTransform {
                        chunk: context.chunk,
                        abort_stream: true,
                        ..Default::default()
                    })
                })
            }),
        ),
    ))
}

/// A law's deployment: its core, its session, and the sink its core's trace
/// records land in.
struct Replay {
    core: LashCore,
    session: lash::LashSession,
    trace: Arc<lash::testing::RecordingTraceSink>,
}

impl Replay {
    /// A session over `provider` and `plugins` whose history holds
    /// [`foreign_replay_message`].
    async fn over(
        id: &str,
        provider: lash::testing::TestProvider,
        plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
        with_foreign_replay: bool,
    ) -> Result<Self> {
        let trace = Arc::new(lash::testing::RecordingTraceSink::new());
        let mut builder = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(provider.into_handle(), mock_llm_profile_spec())
        .trace_sink(trace.clone());
        for plugin in plugins {
            builder = builder.plugin(plugin);
        }
        let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
        let session = created_session(
            &core,
            lash_vocabulary::identity::SessionId::parse(id).expect("nonblank host identity"),
        )
        .await
        .open()
        .await?;
        if with_foreign_replay {
            session
                .admin()
                .state()
                .append_session_nodes(lash_core::AppendSessionNodesRequest {
                    operation_id: "seed-foreign-replay".to_string(),
                    nodes: vec![lash_core::SessionAppendNode::message(
                        foreign_replay_message(),
                    )],
                    requires_ancestor_node_id: None,
                })
                .await?
                .settle_with(
                    &session.admin().commands(),
                    lash::testing::admin_fixture_outcome,
                )
                .await?;
        }
        Ok(Self {
            core,
            session,
            trace,
        })
    }

    async fn run(&self) -> Result<lash::TurnOutput> {
        self.session
            .send(TurnInput::text("continue"))
            .output()
            .await
    }

    fn trace_events(&self) -> Vec<lash_trace::TraceEvent> {
        self.core.flush_trace_sink().expect("flush the trace sink");
        self.trace
            .records()
            .into_iter()
            .map(|record| record.event)
            .collect()
    }
}

fn assert_drop_survived(calls: &[lash::LlmCallRecord], events: &[lash_trace::TraceEvent]) {
    assert_eq!(calls.len(), 1, "{:?}", calls);
    assert_eq!(calls[0].replay_drops.len(), 1);
    assert_eq!(
        calls[0].replay_drops[0].reason,
        lash_core::ProviderReplayDropReason::ForeignRoute
    );
    assert!(events.iter().any(|event| matches!(
        event,
        lash_trace::TraceEvent::ProviderReplayDropped { event }
            if event.replay_kind == lash_trace::TraceProviderReplayKind::Reasoning
                && event.reason == lash_trace::TraceProviderReplayDropReason::ForeignRoute
    )));
}

fn call_failed_with(events: &[lash_trace::TraceEvent], code: &str) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            lash_trace::TraceEvent::LlmCallFailed { error, .. }
                if error.code.as_ref().map(FailureCode::namespaced).as_deref() == Some(code)
        )
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_shaped_completion_preserves_drop_sideband_without_provider_trace() -> Result<()> {
    let provider = lash::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            assert!(request.provider_trace.is_none());
            assert!(!format!("{:?}", request.messages).contains("foreign-request-signature"));
            Ok(text_response("done"))
        })
        .build();
    let replay = Replay::over("replay-completion", provider, Vec::new(), true).await?;

    let report = replay.run().await?;
    let calls =
        lash::testing::model_calls_for_activity(&replay.session.durable(), &report.activities)
            .await;
    let events = replay.trace_events();
    assert_drop_survived(&calls, &events);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, lash_trace::TraceEvent::LlmCallCompleted { .. }))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_shaped_failure_preserves_drop_sideband_and_original_error() -> Result<()> {
    let provider = lash::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            assert!(request.provider_trace.is_none());
            Err(LlmTransportError::new("original provider failure")
                .with_kind(lash_core::ProviderFailureKind::Validation)
                .with_code(FailureCode::provider("original_provider_code")))
        })
        .build();
    let replay = Replay::over("replay-failure", provider, Vec::new(), true).await?;

    let report = replay.run().await?;
    let calls =
        lash::testing::model_calls_for_activity(&replay.session.durable(), &report.activities)
            .await;
    let events = replay.trace_events();
    assert_drop_survived(&calls, &events);
    assert!(
        report
            .activities
            .iter()
            .filter_map(|activity| match &activity.event {
                lash::TurnEvent::Error(error) => error.envelope.as_ref(),
                _ => None,
            })
            .any(|error| {
                error.user_message == "provider call failed"
                    && error.code == Some(FailureCode::provider("original_provider_code"))
            })
    );
    assert!(call_failed_with(&events, "provider:original_provider_code"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_shaped_protocol_abort_rejects_foreign_stream_and_emits_drop() -> Result<()> {
    let provider = lash::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            assert!(request.provider_trace.is_none());
            let events = request.stream_events.expect("streaming driver sender");
            events.send(LlmStreamEvent::Part(LlmOutputPart::Reasoning {
                text: "foreign streamed summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some("foreign-stream-signature".to_string()),
                    origin: Some(foreign_route()),
                    ..Default::default()
                }),
            }));
            events.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "complete block".to_string(),
            }));
            std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
        })
        .build();
    let replay = Replay::over(
        "replay-protocol-abort",
        provider,
        vec![abort_first_chunk("abort-first-chunk")],
        true,
    )
    .await?;

    let report = replay.run().await?;
    let calls =
        lash::testing::model_calls_for_activity(&replay.session.durable(), &report.activities)
            .await;
    let events = replay.trace_events();
    assert_drop_survived(&calls, &events);
    assert!(
        report
            .activities
            .iter()
            .filter_map(|activity| match &activity.event {
                lash::TurnEvent::Error(error) => error.envelope.as_ref(),
                _ => None,
            })
            .any(|error| {
                error.code == Some(lash_core::TurnFailureCode::ProviderReplayOriginConflict.into())
                    && !error.user_message.contains("foreign-stream-signature")
            })
    );
    assert!(call_failed_with(
        &events,
        "lash:provider_replay_origin_conflict"
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, lash_trace::TraceEvent::LlmCallCompleted { .. }))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_shaped_cancellation_preserves_drop_sideband_without_provider_trace() -> Result<()> {
    let started = Arc::new(tokio::sync::Notify::new());
    let provider_started = Arc::clone(&started);
    let provider = lash::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |request| {
            let provider_started = Arc::clone(&provider_started);
            async move {
                assert!(request.provider_trace.is_none());
                request
                    .stream_events
                    .expect("streaming driver sender")
                    .send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                        kind: StreamBlockKind::AssistantText,
                        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                        text: "partial output".to_string(),
                    }));
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                provider_started.notify_one();
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            }
        })
        .build();
    let replay = Replay::over("replay-cancellation", provider, Vec::new(), true).await?;

    let handle = replay.session.send(TurnInput::text("continue")).await?;
    started.notified().await;
    handle.cancel().await?;
    let report = handle.output().await?;
    let calls =
        lash::testing::model_calls_for_activity(&replay.session.durable(), &report.activities)
            .await;
    let events = replay.trace_events();
    assert_drop_survived(&calls, &events);
    assert_eq!(calls[0].attempts.len(), 1);
    assert_eq!(
        calls[0].attempts[0].outcome,
        lash_core::AttemptOutcome::Aborted
    );
    assert_eq!(
        calls[0].attempts[0].protocol_position,
        lash_core::ProtocolPosition::OutputStarted
    );
    assert!(matches!(
        report.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert!(call_failed_with(&events, "lash:cancelled"));
    Ok(())
}

/// A provider failure whose partial carries a foreign replay origin, in
/// before the protocol accepts the stream, fails the call with the typed
/// conflict and keeps the provider's own failure on the attempt. A failure
/// that arrives after the acceptance is the call's tail (FIG-6433):
/// [`protocol_abort_commits_a_complete_cell_despite_a_conflict_free_tail_failure`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirm2_protocol_abort_conflict_retains_a_racing_provider_failure() -> Result<()> {
    let returned = Arc::new(tokio::sync::Notify::new());
    let provider_returned = Arc::clone(&returned);
    let provider = lash::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |request| {
            let provider_returned = Arc::clone(&provider_returned);
            async move {
                let events = request.stream_events.expect("streaming driver sender");
                events.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: "abort now".to_string(),
                }));
                provider_returned.notify_one();
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
                                origin: Some(foreign_route()),
                                ..Default::default()
                            }),
                        }],
                        ..Default::default()
                    }))
            }
        })
        .build();
    let replay = Replay::over(
        "confirm2-abort-conflict-provider-failure",
        provider,
        vec![abort_first_chunk_after_provider_returned(
            "abort-after-provider-failure",
            returned,
        )],
        true,
    )
    .await?;

    let report = replay.run().await?;
    let calls =
        lash::testing::model_calls_for_activity(&replay.session.durable(), &report.activities)
            .await;
    assert!(
        report
            .activities
            .iter()
            .filter_map(|activity| match &activity.event {
                lash::TurnEvent::Error(error) => error.envelope.as_ref(),
                _ => None,
            })
            .any(|error| {
                error.code == Some(lash_core::TurnFailureCode::ProviderReplayOriginConflict.into())
                    && error.user_message == "provider call failed"
            })
    );
    let events = replay.trace_events();
    assert_drop_survived(&calls, &events);
    let issue = report
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::Error(error) => error.envelope.as_ref(),
            _ => None,
        })
        .find(|error| {
            error.code == Some(lash_core::TurnFailureCode::ProviderReplayOriginConflict.into())
        })
        .expect("typed replay-origin conflict issue");
    assert_eq!(
        issue.provider_failure_kind,
        Some(lash_core::ProviderFailureKind::Validation)
    );
    assert_eq!(issue.raw, None);
    let original = calls[0].attempts[0]
        .error
        .as_ref()
        .expect("original provider failure evidence remains attached");
    assert_eq!(original.class, lash_core::ProviderFailureKind::Stream);
    assert_eq!(
        original.code.as_ref().map(|code| code.namespaced()),
        Some("provider:confirm2_original_code".to_string())
    );
    assert_eq!(original.http_status, Some(502));
    assert!(call_failed_with(
        &events,
        "lash:provider_replay_origin_conflict"
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_abort_commits_a_complete_cell_despite_a_conflict_free_tail_failure() -> Result<()>
{
    let provider = lash::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(|request| async move {
            request
                .stream_events
                .expect("streaming driver sender")
                .send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: "complete cell".to_string(),
                }));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Err(LlmTransportError::new("tail transport failure")
                .with_kind(lash_core::ProviderFailureKind::Stream)
                .with_code(FailureCode::provider("tail_transport_failure"))
                .with_http_status(502)
                .with_raw("tail raw evidence"))
        })
        .build();
    let replay = Replay::over(
        "abort-tail-provider-failure",
        provider,
        vec![abort_first_chunk("abort-complete-cell")],
        false,
    )
    .await?;

    let report = replay.run().await?;
    let calls =
        lash::testing::model_calls_for_activity(&replay.session.durable(), &report.activities)
            .await;
    assert!(
        !report
            .activities
            .iter()
            .any(|activity| matches!(&activity.event, lash::TurnEvent::Error(_))),
        "complete cell remains accepted"
    );
    assert!(
        matches!(report.outcome, TurnOutcome::Finished(_)),
        "{:?}",
        report.outcome
    );
    assert_eq!(calls.len(), 1);
    let original = calls[0].attempts[0]
        .error
        .as_ref()
        .expect("tail provider failure remains in the sealed call record");
    assert_eq!(original.class, lash_core::ProviderFailureKind::Stream);
    assert_eq!(
        original.code.as_ref().map(|code| code.namespaced()),
        Some("provider:tail_transport_failure".to_string())
    );
    assert_eq!(original.http_status, Some(502));
    assert!(
        replay
            .trace_events()
            .iter()
            .any(|event| matches!(event, lash_trace::TraceEvent::LlmCallCompleted { .. }))
    );
    Ok(())
}
