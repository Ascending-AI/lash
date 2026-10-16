// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::support::TurnOutcome;
use lash_core::TurnEvent;
#[cfg(feature = "rlm")]
use lash_core::facade_support::LlmTransportError;
#[cfg(feature = "rlm")]
use lash_core::facade_support::{SessionGraphFacadeOps as _, SessionNodeProjection as _};
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use tokio::sync::oneshot;

#[tokio::test]
pub(super) async fn interleaved_standard_parts_keep_order_through_store_history_and_anthropic_request()
-> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::<lash_core::LlmRequest>::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("anthropic")
        .complete({
            let requests = Arc::clone(&requests);
            let calls = Arc::clone(&calls);
            move |request| {
                let requests = Arc::clone(&requests);
                let calls = Arc::clone(&calls);
                async move {
                    requests.lock_recover().push(request);
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Ok(LlmResponse {
                            parts: vec![
                                LlmOutputPart::Text {
                                    text: "before".to_string(),
                                    response_meta: None,
                                },
                                LlmOutputPart::Reasoning {
                                    text: "consider".to_string(),
                                    replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                                        signature: Some("signed-consider".to_string()),
                                        ..Default::default()
                                    }),
                                },
                                LlmOutputPart::Text {
                                    text: "after".to_string(),
                                    response_meta: None,
                                },
                                LlmOutputPart::ToolCall {
                                    call_id: "lookup-1".to_string(),
                                    tool_name: "app_lookup".to_string(),
                                    input_json: "{}".to_string(),
                                    replay: None,
                                },
                            ],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        });
                    }
                    Ok(text_response("done"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("interleaved-standard-order").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;

    session
        .send(TurnInput::text("preserve every part"))
        .output()
        .await?;

    let read_view = committed(&session).await;
    let stored_assistant = read_view
        .messages()
        .iter()
        .find(|message| {
            message.role == lash_core::MessageRole::Assistant
                && message
                    .parts
                    .iter()
                    .any(|part| part.provider_call_id() == Some("lookup-1"))
        })
        .expect("stored interleaved assistant message");
    assert_eq!(
        stored_assistant
            .parts
            .iter()
            .map(|part| part.kind())
            .collect::<Vec<_>>(),
        [
            lash_core::PartKind::Prose,
            lash_core::PartKind::Reasoning,
            lash_core::PartKind::Prose,
            lash_core::PartKind::ToolCall,
        ]
    );

    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2);
    let history_blocks = requests[1]
        .messages
        .iter()
        .find(|message| {
            message.role == LlmRole::Assistant
                && message.blocks.iter().any(|block| {
                    matches!(
                        block,
                        LlmContentBlock::ToolCall { call_id, .. } if call_id == "lookup-1"
                    )
                })
        })
        .expect("provider request assistant history");
    assert!(
        matches!(
            history_blocks.blocks.as_slice(),
            [
                LlmContentBlock::Text { text: before, .. },
                LlmContentBlock::Reasoning { text: reasoning, .. },
                LlmContentBlock::Text { text: after, .. },
                LlmContentBlock::ToolCall { call_id, .. },
            ] if before.as_ref() == "before"
                && reasoning == "consider"
                && after.as_ref() == "\n\nafter"
                && call_id == "lookup-1"
        ),
        "provider history blocks: {:#?}",
        history_blocks.blocks
    );

    let wire = lash_provider_anthropic::testing::serialize_request(
        &requests[1],
        lash_core::provider::CacheRetention::None,
    )
    .expect("serialize Anthropic request");
    let wire_blocks = wire["messages"]
        .as_array()
        .and_then(|messages| {
            messages.iter().find_map(|message| {
                let blocks = message["content"].as_array()?;
                blocks
                    .iter()
                    .any(|block| block["id"] == "lookup-1")
                    .then_some(blocks)
            })
        })
        .expect("Anthropic assistant content blocks");
    assert_eq!(
        wire_blocks
            .iter()
            .map(|block| block["type"].as_str().expect("block type"))
            .collect::<Vec<_>>(),
        ["text", "text", "text", "tool_use"]
    );
    assert_eq!(wire_blocks[0]["text"], "before");
    assert_eq!(wire_blocks[1]["text"], "consider");
    assert_eq!(wire_blocks[2]["text"], "\n\nafter");
    assert_eq!(wire_blocks[3]["id"], "lookup-1");
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_streamed_lash_vm_cell_uses_captured_body_when_final_text_is_raw() -> Result<()> {
    run_async_test_on_stack_budget("rlm-streamed-cell-raw-final-test", || async {
        const RAW_FINAL: &str = "Visible before cell.\n<typescript>\nconst payload = \"```markdown\\ninside\\n```\";\nawait control.finish(\"streamed raw final ok\");\n</typescript>";
        const EXPECTED_CODE: &str = "const payload = \"```markdown\\ninside\\n```\";\nawait control.finish(\"streamed raw final ok\");";

        let provider = crate::testing::TestProvider::builder()
            .kind("stream-raw-final-test")
            .requires_streaming(true)
            .complete(|request| async move {
                let stream = request
                    .stream_events
                    .expect("RLM streaming turn should request provider stream events");
                for chunk in [
                    "Visible before",
                    " cell.\n<type",
                    "script>\nconst payload = \"",
                    "```markdown\\ninside\\n",
                    "```\";\nawait control.finish(",
                    "\"streamed raw final ok\");\n</typescript>",
                ] {
                    stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                        kind: StreamBlockKind::AssistantText,
                        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                        text: chunk.to_string(),
                    }));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: RAW_FINAL.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();

        let core =
            explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
                .serve_test_llm_profile(provider, mock_llm_profile_spec())
                .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(
                crate::SessionId::parse("rlm-streamed-raw-final-cell")
                    .expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;
        let events = Arc::new(RecordingEvents::default());

        let result = session
            .send(TurnInput::text("say hi"))
            .output_into(events.as_ref())
            .await?;

        assert!(matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::Finished { .. })
        ));
        assert_eq!(
            result.finished().map(|(_, value)| value),
            Some(&serde_json::json!("streamed raw final ok"))
        );

        let events = events.snapshot().await;
        let prose = assistant_prose(&events);
        assert_eq!(prose, "Visible before cell.\n");
        assert!(!prose.contains("<typescript>"));
        assert!(!prose.contains("finish"));
        assert!(!prose.contains("```markdown"));

        let code_started = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::CodeBlockStarted { .. }))
            .expect("code started");
        let TurnEvent::CodeBlockStarted { language, code, .. } = &code_started.event else {
            unreachable!();
        };
        assert_eq!(language, "typescript");
        assert_eq!(code, EXPECTED_CODE);
        assert!(!code.contains("<typescript>"));

        let code_completed = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::CodeBlockCompleted { .. }))
            .expect("code completed");
        let TurnEvent::CodeBlockCompleted { result, .. } = &code_completed.event else {
            unreachable!();
        };
        assert!(!result.is_failed());

        let terminal_output = events
            .iter()
            .find(|event| matches!(&event.event, TurnEvent::Finished { .. }))
            .expect("terminal output");
        let TurnEvent::Finished { value, .. } = &terminal_output.event else {
            unreachable!();
        };
        assert_eq!(value, &serde_json::json!("streamed raw final ok"));
        Ok(())
    })
}

/// Budgets for a call whose cell is accepted mid-stream: a short stop grace,
/// which the tail's drain must outlast, and `model_total` as the call's
/// deadline.
#[cfg(feature = "rlm")]
fn tail_budgets(model_total: std::time::Duration) -> lash_core::ExecutionBudgets {
    lash_core::ExecutionBudgets::new(lash_core::ExecutionBudgetsConfig {
        model_total,
        stop_grace: TAIL_STOP_GRACE,
        provider: lash_core::ProviderAttemptLimits::new(model_total, model_total, model_total, 1)
            .expect("provider limits"),
        ..lash_core::ExecutionBudgetsConfig::recommended()
    })
    .expect("tail budgets")
}

#[cfg(feature = "rlm")]
const TAIL_STOP_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(feature = "rlm")]
pub(super) async fn rlm_abort_drain_core(
    provider: ProviderHandle,
    model_total: std::time::Duration,
) -> Result<LashCore> {
    explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
        .execution_budgets(tail_budgets(model_total))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
}

/// The first cell of an RLM answer, as the provider streams it.
#[cfg(feature = "rlm")]
fn streamed_cell(stream: &lash_core::llm::types::LlmEventSender, value: &str) {
    stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
        kind: StreamBlockKind::AssistantText,
        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
        text: format!("<typescript>\nawait control.finish(\"{value}\");\n</typescript>\n"),
    }));
}

/// Hands the provider the turn's first `CodeBlockStarted`: the accepted cell
/// is running. Records every activity.
#[cfg(feature = "rlm")]
#[derive(Default)]
struct CellStarted {
    started: StdMutex<Option<oneshot::Sender<()>>>,
    events: RecordingEvents,
}

#[cfg(feature = "rlm")]
#[async_trait]
impl TurnActivitySink for CellStarted {
    async fn emit(&self, activity: TurnActivity) {
        if matches!(activity.event, TurnEvent::CodeBlockStarted { .. })
            && let Some(started) = self.started.lock_recover().take()
        {
            let _ = started.send(());
        }
        self.events.emit(activity).await;
    }
}

/// Observes the provider's future as a host decorator around it does: it
/// either ran to its end or was dropped first.
#[cfg(feature = "rlm")]
struct DecoratedFuture {
    ended: Arc<std::sync::atomic::AtomicBool>,
    dropped_early: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(feature = "rlm")]
impl DecoratedFuture {
    fn end(self) {
        self.ended.store(true, Ordering::SeqCst);
    }
}

#[cfg(feature = "rlm")]
impl Drop for DecoratedFuture {
    fn drop(&mut self) {
        if !self.ended.load(Ordering::SeqCst) {
            self.dropped_early.store(true, Ordering::SeqCst);
        }
    }
}

/// FIG-6433: a call whose cell the protocol accepts mid-stream still
/// commits the usage its provider reports after the tail, later than the
/// stop grace: the cell runs first, the provider future runs to its end,
/// and the call record and the turn's totals carry the usage.
#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_accepted_cell_commits_the_usage_its_tail_reports() -> Result<()> {
    run_async_test_on_stack_budget("rlm-tail-usage", || async {
        let (started_tx, started_rx) = oneshot::channel();
        let started_rx = Arc::new(StdMutex::new(Some(started_rx)));
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped_early = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-tail-usage")
            .requires_streaming(true)
            .complete({
                let ended = Arc::clone(&ended);
                let dropped_early = Arc::clone(&dropped_early);
                move |request| {
                    let started = started_rx.lock_recover().take();
                    let decorated = DecoratedFuture {
                        ended: Arc::clone(&ended),
                        dropped_early: Arc::clone(&dropped_early),
                    };
                    async move {
                        let stream = request.stream_events.expect("stream events");
                        streamed_cell(&stream, "tail drained");
                        stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                            kind: StreamBlockKind::AssistantText,
                            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                            text: "Predicted continuation.".to_string(),
                        }));
                        // The usage chunk waits for the accepted cell to run,
                        // then for longer than the stop grace.
                        started.expect("one call").await.expect("the cell started");
                        tokio::time::sleep(TAIL_STOP_GRACE * 3).await;
                        let usage = lash_core::llm::types::LlmUsage {
                            input_tokens: 17,
                            output_tokens: 5,
                            ..Default::default()
                        };
                        stream.send(LlmStreamEvent::Usage(usage.clone()));
                        decorated.end();
                        Ok(LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: "<typescript>\nawait control.finish(\"tail drained\");\n</typescript>\nPredicted continuation.".to_string(),
                                response_meta: None,
                            }],
                            usage,
                            terminal_reason: lash_core::LlmTerminalReason::Stop,
                            ..LlmResponse::default()
                        })
                    }
                }
            })
            .build()
            .into_handle();
        let core = rlm_abort_drain_core(provider, std::time::Duration::from_secs(60)).await?;
        let session = core
            .session(crate::SessionId::parse("rlm-tail-usage").expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?;
        let sink = CellStarted {
            started: StdMutex::new(Some(started_tx)),
            ..CellStarted::default()
        };

        let result = session
            .send(TurnInput::text("finish"))
            .output_into(&sink)
            .await?;

        assert_eq!(
            result.finished().map(|(_, value)| value),
            Some(&serde_json::json!("tail drained"))
        );
        assert!(
            ended.load(Ordering::SeqCst),
            "the provider future ran to its end"
        );
        assert!(
            !dropped_early.load(Ordering::SeqCst),
            "the provider future was never dropped before its end"
        );
        let record = result.llm_calls.first().expect("the call's record");
        let attempt = record.attempts.last().expect("the call's attempt");
        assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Completed);
        assert_eq!(
            attempt.usage.as_ref().map(|usage| usage.input_tokens),
            Some(17)
        );
        assert_eq!(
            attempt.usage.as_ref().map(|usage| usage.output_tokens),
            Some(5)
        );
        assert_eq!(result.usage.input_tokens, 17);
        assert_eq!(result.usage.output_tokens, 5);
        let committed = committed(&session).await;
        assert_eq!(committed.token_usage().input_tokens, 17);
        assert_eq!(committed.token_usage().output_tokens, 5);
        // The cell ran before the call's record, which waits for the usage.
        let events = sink.events.snapshot().await;
        let cell_started = events
            .iter()
            .position(|event| matches!(event.event, TurnEvent::CodeBlockStarted { .. }))
            .expect("the cell started");
        let call_recorded = events
            .iter()
            .position(|event| matches!(event.event, TurnEvent::ModelCallRecorded { .. }))
            .expect("the call was recorded");
        assert!(cell_started < call_recorded);
        Ok(())
    })
}

/// FIG-6433: a call whose cell was accepted mid-stream and whose usage
/// never arrives, because its deadline passes or its stream fails, has
/// unknown usage on its record and in the turn's totals, never zero.
#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_accepted_cell_without_tail_usage_is_unknown_never_zero() -> Result<()> {
    run_async_test_on_stack_budget("rlm-tail-unknown", || async {
        for case in ["deadline", "stream-error"] {
            let provider = crate::testing::TestProvider::builder()
                .kind("rlm-tail-unknown")
                .requires_streaming(true)
                .complete(move |request| async move {
                    let stream = request.stream_events.expect("stream events");
                    streamed_cell(&stream, "unknown usage");
                    if case == "deadline" {
                        return std::future::pending().await;
                    }
                    tokio::time::sleep(TAIL_STOP_GRACE * 3).await;
                    Err(LlmTransportError::new("the stream broke after the cell")
                        .with_kind(lash_core::ProviderFailureKind::Stream)
                        .with_retry_verdict(
                            lash_core::llm::transport::TransportRetryVerdict::NotRetryable,
                        ))
                })
                .build()
                .into_handle();
            let core = rlm_abort_drain_core(provider, std::time::Duration::from_secs(2)).await?;
            let session = core
                .session(
                    crate::SessionId::parse(format!("rlm-tail-unknown-{case}"))
                        .expect("nonblank host identity"),
                )
                .created()
                .await
                .open()
                .await?;

            let result = session.send(TurnInput::text("finish")).output().await?;

            assert_eq!(
                result.finished().map(|(_, value)| value),
                Some(&serde_json::json!("unknown usage")),
                "{case}"
            );
            let record = result.result.llm_calls.first().expect("the call's record");
            let attempt = record.attempts.last().expect("the call's attempt");
            assert_eq!(attempt.usage, None, "{case}");
            assert!(record.usage_unknown(), "{case}");
            let unknown = lash_core::llm::types::LlmUsage::unknown_call();
            assert_eq!(result.result.usage, unknown, "{case}");
            assert_eq!(committed(&session).await.token_usage(), &unknown, "{case}");
        }
        Ok(())
    })
}

/// FIG-6547: a host decorator receives a terminal for every dispatched
/// attempt, including deadline and cancellation while a cell's tail drains.
#[cfg(feature = "rlm")]
#[derive(Debug)]
struct CountingProvider {
    inner: Box<dyn lash_core::provider::Provider>,
    terminals: Arc<
        StdMutex<
            Vec<(
                lash_core::provider::ProviderAttempt,
                lash_core::provider::AttemptTerminal,
            )>,
        >,
    >,
    returned: Arc<StdMutex<Vec<(u32, bool, lash_core::llm::types::LlmUsage)>>>,
    terminal_delivered: Arc<tokio::sync::Notify>,
}

#[cfg(feature = "rlm")]
#[async_trait]
impl lash_core::provider::Provider for CountingProvider {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn route_identity(&self, model: &str) -> lash_core::llm::types::ProviderRouteIdentity {
        self.inner.route_identity(model)
    }
    fn options(&self) -> lash_core::provider::ProviderOptions {
        self.inner.options()
    }
    fn set_options(&mut self, options: lash_core::provider::ProviderOptions) {
        self.inner.set_options(options);
    }
    fn serialize_config(&self) -> serde_json::Value {
        self.inner.serialize_config()
    }
    fn requires_streaming(&self) -> bool {
        self.inner.requires_streaming()
    }
    fn generation_retry_guarantee(
        &self,
        context: &lash_core::llm::types::ResponseContext,
        template: &lash_core::llm::types::RecordedRequestTemplate,
    ) -> lash_core::provider::GenerationRetryGuarantee {
        self.inner.generation_retry_guarantee(context, template)
    }
    async fn send(
        &mut self,
        body: &lash_core::llm::types::LiveRequestBody,
        context: lash_core::llm::types::ResponseContext,
    ) -> std::result::Result<LlmResponse, LlmTransportError> {
        let ordinal = context.scope.attempt.expect("Lash assigns the ordinal");
        let result = self.inner.send(body, context).await;
        let usage = match &result {
            Ok(response) => response.usage.clone(),
            Err(error) => error
                .partial_response
                .as_ref()
                .map(|response| response.usage.clone())
                .unwrap_or_default(),
        };
        self.returned
            .lock_recover()
            .push((ordinal, result.is_ok(), usage));
        result
    }
    async fn attempt_ended(
        &mut self,
        attempt: lash_core::provider::ProviderAttempt,
        terminal: lash_core::provider::AttemptTerminal,
    ) {
        self.terminals
            .lock_recover()
            .push((attempt.clone(), terminal.clone()));
        self.terminal_delivered.notify_one();
        self.inner.attempt_ended(attempt, terminal).await;
    }
    fn clone_boxed(&self) -> Box<dyn lash_core::provider::Provider> {
        Box::new(Self {
            inner: self.inner.clone_boxed(),
            terminals: self.terminals.clone(),
            returned: self.returned.clone(),
            terminal_delivered: self.terminal_delivered.clone(),
        })
    }
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn every_abandoned_attempt_reaches_the_provider_with_its_partial_receipt() -> Result<()>
{
    run_async_test_on_stack_budget("abandoned-provider-attempt", || async {
        use lash_core::provider::{AttemptEndReason, AttemptTerminal};
        for case in [
            "deadline-streaming",
            "deadline-tail",
            "cancel-streaming",
            "cancel-tail",
            "stream-hook",
            "provider-panic",
        ] {
            let terminals = Arc::new(StdMutex::new(Vec::new()));
            let returned = Arc::new(StdMutex::new(Vec::new()));
            let usage = lash_core::llm::types::LlmUsage {
                input_tokens: 17,
                output_tokens: 5,
                cache_read_input_tokens: 13,
                cache_write_input_tokens: 7,
                reasoning_output_tokens: 3,
                ..Default::default()
            };
            let provider = crate::testing::TestProvider::builder()
                .kind("abandoned-provider-attempt")
                .requires_streaming(true)
                .complete({ let usage = usage.clone(); move |request| {
                    let usage = usage.clone();
                    async move {
                        let stream = request.stream_events.expect("stream events");
                        stream.send(LlmStreamEvent::Evidence(lash_core::llm::types::LlmStreamEvidence {
                            response_started: true,
                            provider_usage: Some(serde_json::json!({"input_tokens":17,"output_tokens":5,"cost":0.04})),
                            response_metadata: [("header:x-opper-cost".into(), serde_json::json!("0.04")), ("body:/usage/cost".into(), serde_json::json!(0.04))].into(),
                            execution_evidence: Some(lash_core::llm::types::ExecutionEvidence { served_model: Some("served-model".into()), provider_response_id: Some("response-1".into()), ..Default::default() }),
                            ..Default::default()
                        }));
                        stream.send(LlmStreamEvent::Usage(usage));
                        if case.ends_with("tail") { streamed_cell(&stream, "partial receipt"); }
                        else { stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta { kind: StreamBlockKind::AssistantText, block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0), text: "Let me think.".into() })); }
                        if case == "provider-panic" { panic!("provider panicked after receiving usage"); }
                        std::future::pending().await
                    }
                }})
                .build();
            let provider = ProviderHandle::new(lash_core::provider::ProviderComponents::new(
                Box::new(CountingProvider {
                    inner: Box::new(provider),
                    terminals: terminals.clone(),
                    returned: returned.clone(),
                    terminal_delivered: Arc::default(),
                }),
            ));
            let total =
                std::time::Duration::from_secs(if case.starts_with("cancel") { 60 } else { 2 });
            let mut builder = explicit_ephemeral_facets(rlm_core_builder_over(
                sqlite_memory_store_backend().await,
            ))
            .execution_budgets(tail_budgets(total))
            .serve_test_llm_profile(provider, mock_llm_profile_spec());
            if case == "stream-hook" {
                builder = builder.plugin(Arc::new(StaticPluginFactory::new(
                    lash_core::plugin::PluginDeclaration::initial("failing-stream-consumer"),
                    lash_core::facade_support::PluginSpec::new().with_assistant_stream(
                        lash_core::hook_key!("fail-first-chunk"),
                        Arc::new(|_| {
                            Box::pin(async {
                                Err(lash_core::PluginError::Session("stream hook failed".into()))
                            })
                        }),
                    ),
                )));
            }
            let core = builder.build(crate::testing::runtime_lease_owner())?;
            let session = core
                .session(
                    crate::SessionId::parse(format!("abandoned-provider-{case}"))
                        .expect("identity"),
                )
                .created()
                .await
                .open()
                .await?;
            let handle = session.send(TurnInput::text("finish")).await?;
            if case.starts_with("cancel") {
                let mut events = handle.events();
                while let Some(activity) = events.next_activity().await {
                    let event = activity?.event;
                    if (case == "cancel-tail"
                        && matches!(event, TurnEvent::CodeBlockStarted { .. }))
                        || (case == "cancel-streaming"
                            && matches!(
                                event,
                                TurnEvent::StreamBlock(StreamBlockEvent::Delta { .. })
                            ))
                    {
                        break;
                    }
                }
                handle
                    .cancel()
                    .mode(crate::TurnCancelMode::Immediate)
                    .await?;
            }
            let output = handle.output().await;
            if case != "provider-panic" {
                output?;
            }
            assert!(
                returned.lock_recover().is_empty(),
                "{case}: the stalled send returned nothing"
            );
            let observed = terminals.lock_recover();
            assert_eq!(observed.len(), 1, "{case}: one provider terminal");
            let (
                attempt,
                AttemptTerminal::Abandoned {
                    reason,
                    drain_timed_out,
                    partial_response,
                },
            ) = &observed[0];
            assert_eq!(attempt.ordinal, 1);
            assert_eq!(attempt.scope.attempt, Some(1));
            assert_eq!(attempt.call_id.0, attempt.scope.request_id);
            assert!(attempt.scope.llm_call_id.is_some());
            let expected = match case {
                "stream-hook" => AttemptEndReason::StreamConsumerFailed,
                "provider-panic" => AttemptEndReason::ProviderPanicked,
                _ if case.starts_with("cancel") => AttemptEndReason::ImmediateCancel,
                _ => AttemptEndReason::ModelDeadline,
            };
            assert_eq!(*reason, expected, "{case}");
            assert_eq!(*drain_timed_out, case != "provider-panic", "{case}");
            let partial = partial_response
                .as_ref()
                .expect("the response received before abandonment");
            assert_eq!(partial.usage, usage, "{case}");
            assert_eq!(
                partial.provider_usage,
                Some(serde_json::json!({"input_tokens":17,"output_tokens":5,"cost":0.04})),
                "{case}"
            );
            assert_eq!(
                partial.response_metadata["header:x-opper-cost"], "0.04",
                "{case}"
            );
            assert_eq!(
                partial.response_metadata["body:/usage/cost"], 0.04,
                "{case}"
            );
            let evidence = partial.execution_evidence.as_ref().expect("identity");
            assert_eq!(evidence.served_model.as_deref(), Some("served-model"));
            assert_eq!(evidence.provider_response_id.as_deref(), Some("response-1"));
        }
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn returned_provider_attempts_settle_once_without_an_abandoned_terminal() -> Result<()> {
    run_async_test_on_stack_budget("returned-provider-attempt", || async {
        for case in ["completed", "accepted-mid-stream", "retry"] {
            let terminals = Arc::new(StdMutex::new(Vec::new()));
            let returned = Arc::new(StdMutex::new(Vec::new()));
            let provider = crate::testing::TestProvider::builder().kind("returned-provider-attempt").requires_streaming(case == "accepted-mid-stream")
                .options(lash_core::provider::ProviderOptions { reliability: lash_core::provider::ProviderReliability::default().max_attempts(Some(2)).base_delay_ms(0).max_delay_ms(0), ..Default::default() })
                .generation_retry_guarantee(if case == "retry" { lash_core::provider::GenerationRetryGuarantee::Idempotent } else { lash_core::provider::GenerationRetryGuarantee::None })
                .complete(move |request| async move {
                if case == "retry" && request.scope.attempt == Some(1) {
                    return Err(LlmTransportError::new("failed after partial usage")
                        .with_kind(lash_core::ProviderFailureKind::Stream)
                        .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::RetryableTransient)
                        .with_partial_response(LlmResponse { usage: lash_core::llm::types::LlmUsage { input_tokens: 2, output_tokens: 1, ..Default::default() }, provider_usage: Some(serde_json::json!({"input_tokens":2,"output_tokens":1})), ..Default::default() }));
                }
                if case == "accepted-mid-stream" { streamed_cell(&request.stream_events.expect("stream events"), "settled"); tokio::time::sleep(TAIL_STOP_GRACE * 3).await; }
                Ok(LlmResponse { parts: vec![LlmOutputPart::Text { text: "<typescript>\nawait control.finish(\"settled\");\n</typescript>\n".into(), response_meta: None }], usage: lash_core::llm::types::LlmUsage { input_tokens: 17, output_tokens: 5, ..Default::default() }, terminal_reason: lash_core::LlmTerminalReason::Stop, ..Default::default() })
            }).build();
            let provider = ProviderHandle::new(lash_core::provider::ProviderComponents::new(
                Box::new(CountingProvider {
                    inner: Box::new(provider),
                    terminals: terminals.clone(),
                    returned: returned.clone(),
                    terminal_delivered: Arc::default(),
                }),
            ));
            let total = std::time::Duration::from_secs(60);
            let budgets = lash_core::ExecutionBudgets::new(lash_core::ExecutionBudgetsConfig {
                model_total: total,
                stop_grace: TAIL_STOP_GRACE,
                provider: lash_core::ProviderAttemptLimits::new(total, total, total, 2)
                    .expect("provider limits"),
                ..lash_core::ExecutionBudgetsConfig::recommended()
            })
            .expect("budgets");
            let core = explicit_ephemeral_facets(rlm_core_builder_over(
                sqlite_memory_store_backend().await,
            ))
            .execution_budgets(budgets)
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .build(crate::testing::runtime_lease_owner())?;
            let session = core
                .session(
                    crate::SessionId::parse(format!("returned-provider-{case}")).expect("identity"),
                )
                .created()
                .await
                .open()
                .await?;
            let output = session.send(TurnInput::text("finish")).output().await?;
            assert_eq!(
                output.finished().map(|(_, value)| value),
                Some(&serde_json::json!("settled")),
                "{case}"
            );
            assert!(
                terminals.lock_recover().is_empty(),
                "{case}: returned attempts have no callback"
            );
            let observed = returned.lock_recover();
            let outcomes: Vec<_> = observed
                .iter()
                .map(|(ordinal, success, usage)| {
                    (*ordinal, *success, usage.input_tokens, usage.output_tokens)
                })
                .collect();
            assert_eq!(
                outcomes,
                if case == "retry" {
                    vec![(1, false, 2, 1), (2, true, 17, 5)]
                } else {
                    vec![(1, true, 17, 5)]
                },
                "{case}"
            );
        }
        Ok(())
    })
}

/// A tool's cancellation drops its direct-completion awaiter. The owned
/// provider task must still reach the decorator with the partial usage.
#[cfg(feature = "rlm")]
#[test]
pub(super) fn cancelling_a_tool_settles_its_nested_direct_provider_attempt() -> Result<()> {
    run_async_test_on_stack_budget("cancel-direct-provider", || async {
        let terminals = Arc::new(StdMutex::new(Vec::new()));
        let returned = Arc::new(StdMutex::new(Vec::new()));
        let terminal_delivered = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let provider = crate::testing::TestProvider::builder()
            .kind("nested-direct-terminal")
            .complete({
                let entered = entered.clone();
                move |request| {
                    let entered = entered.clone();
                    async move {
                        if last_user_text(&request).contains("raw prompt") {
                            request.stream_events.as_ref().expect("stream events").send(
                                LlmStreamEvent::Usage(lash_core::llm::types::LlmUsage {
                                    input_tokens: 2,
                                    output_tokens: 1,
                                    ..Default::default()
                                }),
                            );
                            entered.notify_one();
                            return std::future::pending().await;
                        }
                        Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "direct-probe".into(),
                                tool_name: "direct_probe".into(),
                                input_json: "{}".into(),
                                replay: None,
                            }],
                            ..Default::default()
                        })
                    }
                }
            })
            .requires_streaming(true)
            .build();
        let provider = ProviderHandle::new(lash_core::provider::ProviderComponents::new(Box::new(
            CountingProvider {
                inner: Box::new(provider),
                terminals: terminals.clone(),
                returned: returned.clone(),
                terminal_delivered: terminal_delivered.clone(),
            },
        )));
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(
            crate::tool_provider_key!("direct-probe"),
            Arc::new(crate::tests::direct_completion::DirectProbe),
        )
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(crate::SessionId::parse("cancel-nested-direct").expect("identity"))
            .created()
            .await
            .open()
            .await?;
        let handle = session
            .send(TurnInput::text("ask the model directly"))
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .expect("the nested provider was entered");
        handle
            .cancel()
            .mode(crate::TurnCancelMode::Immediate)
            .await?;
        handle.output().await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            terminal_delivered.notified(),
        )
        .await
        .expect("the dropped direct awaiter still delivers its provider terminal");
        let observed = terminals.lock_recover();
        assert_eq!(observed.len(), 1);
        let (
            _,
            lash_core::provider::AttemptTerminal::Abandoned {
                reason,
                partial_response,
                ..
            },
        ) = &observed[0];
        assert_eq!(
            *reason,
            lash_core::provider::AttemptEndReason::ImmediateCancel
        );
        let partial = partial_response.as_ref().expect("partial usage");
        assert_eq!(
            (partial.usage.input_tokens, partial.usage.output_tokens),
            (2, 1)
        );
        assert_eq!(
            returned.lock_recover().len(),
            1,
            "only the outer model call returned"
        );
        Ok(())
    })
}

/// A provider reset after the cell was accepted never erases the cell: the
/// reset belongs to the call's tail.
#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_abort_drain_ignores_a_late_attempt_reset() -> Result<()> {
    run_async_test_on_stack_budget("rlm-abort-drain-attempt-reset", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-abort-reset")
            .requires_streaming(true)
            .complete(|request| async move {
                let stream = request.stream_events.expect("stream events");
                streamed_cell(&stream, "cell survived reset");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                stream.send(LlmStreamEvent::AttemptReset);
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            })
            .build()
            .into_handle();
        let core = rlm_abort_drain_core(provider, std::time::Duration::from_secs(2)).await?;
        let session = core
            .session(crate::SessionId::parse("rlm-abort-reset").expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?;

        let result = session.send(TurnInput::text("finish")).output().await?;

        assert_eq!(
            result.finished().map(|(_, value)| value),
            Some(&serde_json::json!("cell survived reset"))
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_turn_without_interruption_or_usage_preserves_absent_usage() -> Result<()> {
    run_async_test_on_stack_budget("rlm-zero-usage-no-row", || async {
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-zero-usage")
            .complete(|_request| async move {
                Ok(LlmResponse {
                    parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                        text: "<typescript>\nawait control.finish(\"quiet\");\n</typescript>\n"
                            .to_string(),
                        response_meta: None,
                    }],
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();
        let core = rlm_abort_drain_core(provider, std::time::Duration::from_secs(60)).await?;
        let session = core
            .session(crate::SessionId::parse("rlm-zero-usage").expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?;
        let result = session.send(TurnInput::text("finish")).output().await?;
        assert_eq!(
            result.finished().map(|(_, value)| value),
            Some(&serde_json::json!("quiet"))
        );
        let attempt = result
            .result
            .llm_calls
            .first()
            .and_then(|record| record.attempts.first())
            .expect("completed attempt");
        assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Completed);
        assert_eq!(
            attempt.usage_disposition(),
            lash_core::AttemptUsageOutcome::UnreportedByProvider
        );

        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_tool_calls_stream_from_live_exec_boundary() -> Result<()> {
    run_async_test_on_stack_budget("rlm-live-exec-boundary-test", || {
        rlm_tool_calls_stream_from_live_exec_boundary_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_tool_calls_stream_from_live_exec_boundary_inner() -> Result<()> {
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                queued_text_provider(vec![typescript_block(
                    r#"const value = await tools.app_lookup({});
await control.finish("done");"#,
                )]),
                mock_llm_profile_spec(),
            )
            .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("rlm-live-tool-events").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .send(TurnInput::text("use tool"))
        .output_into(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::Finished { .. })
    ));
    // The lookup, then the cell's `control.finish`.
    assert_eq!(result.tool_calls.len(), 2);
    assert_eq!(result.tool_calls[0].tool, "app_lookup");
    assert_eq!(result.tool_calls[0].args, serde_json::json!({}));
    assert_eq!(
        result.tool_calls[0].output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
    let events = events.snapshot().await;
    let code_started = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::CodeBlockStarted { .. }))
        .expect("code started");
    let tool_started = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallStarted { .. }))
        .expect("tool started");
    let tool_completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completed");
    let code_completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::CodeBlockCompleted { .. }))
        .expect("code completed");
    let terminal_output = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::Finished { .. }))
        .expect("terminal output");
    assert!(code_started < tool_started);
    assert!(tool_started < tool_completed);
    assert!(tool_completed < code_completed);
    assert!(code_completed < terminal_output);
    assert!(!events[code_completed + 1..].iter().any(|event| matches!(
        &event.event,
        TurnEvent::ToolCallStarted { .. } | TurnEvent::ToolCallCompleted { .. }
    )));

    let TurnEvent::ToolCallCompleted {
        call_id,
        output,
        graph_key: tool_completed_graph_key,
        ..
    } = &events[tool_completed].event
    else {
        unreachable!();
    };
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
    let TurnEvent::CodeBlockStarted {
        graph_key: started_graph_key,
        ..
    } = &events[code_started].event
    else {
        unreachable!();
    };
    let encoded_address = started_graph_key
        .as_deref()
        .and_then(|key| key.strip_prefix("effect:"))
        .unwrap_or_else(|| {
            panic!("missing foreground effect address on CodeBlockStarted: {started_graph_key:?}")
        });
    let mut scope =
        serde_json::Deserializer::from_str(encoded_address).into_iter::<serde_json::Value>();
    let scope_value = scope
        .next()
        .transpose()?
        .expect("foreground graph key carries its execution scope");
    let replay_key = encoded_address
        .get(scope.byte_offset()..)
        .and_then(|suffix| suffix.strip_prefix(':'))
        .map(serde_json::from_str::<String>)
        .transpose()?
        .expect("foreground graph key carries its replay key");
    assert_eq!(scope_value["version"], 2);
    assert_eq!(scope_value["kind"], "turn");
    assert_eq!(scope_value["session_id"], "rlm-live-tool-events");
    let execution_id = scope_value["execution_id"]
        .as_str()
        .expect("foreground turn scope carries its execution id");
    assert_eq!(
        replay_key,
        format!("rlm-live-tool-events:{execution_id}:1:0:exec_code:3")
    );
    let TurnEvent::CodeBlockCompleted {
        language,
        result: cell_outcome,
        tool_call_ids,
        graph_key: completed_graph_key,
        ..
    } = &events[code_completed].event
    else {
        unreachable!();
    };
    assert_eq!(language, "typescript");
    assert!(!cell_outcome.is_failed());
    assert_eq!(Some(call_id), tool_call_ids.first());
    // The lookup, then the cell's `control.finish`.
    assert_eq!(tool_call_ids.len(), 2);
    assert_eq!(completed_graph_key, started_graph_key);
    // Task 4: the RLM tool call carries the enclosing block's graph_key for
    // structural containment.
    let TurnEvent::ToolCallStarted {
        graph_key: tool_started_graph_key,
        ..
    } = &events[tool_started].event
    else {
        unreachable!();
    };
    assert_eq!(tool_started_graph_key, started_graph_key);
    assert_eq!(tool_completed_graph_key, started_graph_key);
    let read_view = committed(&session).await;
    assert!(
        read_view.messages().iter().all(|message| message
            .parts
            .iter()
            .all(|part| part.call_id() != tool_call_ids.first())),
        "live RLM tool calls should not be persisted as message history"
    );
    assert_eq!(
        read_view
            .session_graph()
            .clone()
            .active_path_nodes()
            .into_iter()
            .filter_map(|node| node.event())
            .filter(|event| matches!(event, lash_core::SessionHistoryRecord::Conversation(_)))
            .count(),
        read_view.messages().len()
    );
    let TurnEvent::Finished { value, .. } = &events[terminal_output].event else {
        unreachable!();
    };
    assert_eq!(value, &serde_json::json!("done"));
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_recovered_tool_failure_remains_in_turn_accounting() -> Result<()> {
    run_async_test_on_stack_budget("rlm-recovered-tool-failure-test", || async {
        let core =
            explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
                .serve_test_llm_profile(
                    queued_text_provider(vec![typescript_block(
                        r#"let failure;
try {
  failure = await tools.app_lookup({});
} catch (error) {
  failure = error;
}
await control.finish("recovered");"#,
                    )]),
                    mock_llm_profile_spec(),
                )
                .tools(
                    crate::tool_provider_key!("tools"),
                    Arc::new(FailingAppTools),
                )
                .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(
                crate::SessionId::parse("rlm-recovered-tool-failure")
                    .expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;

        let result = session
            .send(TurnInput::text("recover the tool failure"))
            .output()
            .await?
            .result;

        assert!(matches!(
            result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::Finished { .. })
        ));
        assert_eq!(
            result.finished().map(|(_, value)| value),
            Some(&serde_json::json!("recovered"))
        );
        // The failed lookup, then the `control.finish` that recovered.
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_calls[0].tool, "app_lookup");
        assert!(!result.tool_calls[0].output.is_success());
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_code_block_aggregate_lists_every_collected_tool_call() -> Result<()> {
    run_async_test_on_stack_budget("rlm-aggregate-tool-ids-test", || {
        rlm_code_block_aggregate_lists_every_collected_tool_call_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_code_block_aggregate_lists_every_collected_tool_call_inner() -> Result<()> {
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                queued_text_provider(vec![typescript_block(
                    r#"const a = await tools.app_lookup({});
const b = await tools.app_lookup({});
await control.finish("done");"#,
                )]),
                mock_llm_profile_spec(),
            )
            .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("rlm-aggregate-tool-ids").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .send(TurnInput::text("use tools"))
        .output_into(events.as_ref())
        .await?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::Finished { .. })
    ));
    let events = events.snapshot().await;

    // Every collected RLM tool record carries its call id, so the code
    // block's `tool_call_ids` aggregate lists each call.
    let completed_ids = events
        .iter()
        .filter_map(|event| match &event.event {
            TurnEvent::ToolCallCompleted { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completed_ids.len(),
        3,
        "expected two tool completions and the `control.finish`"
    );

    let tool_call_ids = events
        .iter()
        .find_map(|event| match &event.event {
            TurnEvent::CodeBlockCompleted { tool_call_ids, .. } => Some(tool_call_ids.clone()),
            _ => None,
        })
        .expect("code block completed");
    assert_eq!(tool_call_ids.len(), completed_ids.len());
    for call_id in completed_ids {
        assert!(
            tool_call_ids.contains(&call_id),
            "code block aggregate must list collected tool call {call_id}"
        );
    }
    Ok(())
}

/// FIG-2777: a provider tool call on the cell channel — whose request declares
/// no tools — is malformed provider output, so it takes the extraction-failure
/// repair round like a reply with no usable cell. The turn stops typed only if
/// the repair budget dies with it.
#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_native_provider_tool_call_repairs_and_the_next_cell_finishes() -> Result<()> {
    run_async_test_on_stack_budget("rlm-native-tool-contract-test", || async {
        let trace = Arc::new(crate::testing::RecordingTraceSink::new());
        let repair_request = Arc::new(std::sync::Mutex::new(None));
        let responses = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from([
            LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "native-call-1".to_string(),
                    tool_name: "native_lookup".to_string(),
                    input_json: r#"{"query":"forbidden"}"#.to_string(),
                    replay: None,
                }],
                terminal_reason: lash_core::LlmTerminalReason::ToolUse,
                response_metadata: Default::default(),
                ..LlmResponse::default()
            },
            text_response(&typescript_block("await control.finish(1);")),
        ])));
        let provider = crate::testing::TestProvider::builder()
            .kind("native-tool-call-under-rlm")
            .complete({
                let repair_request = Arc::clone(&repair_request);
                move |request| {
                    let responses = Arc::clone(&responses);
                    let repair_request = Arc::clone(&repair_request);
                    async move {
                        if responses.lock().await.len() == 1 {
                            *repair_request.lock().unwrap() = Some(format!("{request:?}"));
                        }
                        Ok(responses.lock().await.pop_front().expect("queued response"))
                    }
                }
            })
            .build()
            .into_handle();
        let core =
            explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
                .serve_test_llm_profile(provider, mock_llm_profile_spec())
                .trace_sink(trace.clone())
                .content_policy(lash_trace::ContentPolicy::captured())
                .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(
                crate::SessionId::parse("rlm-native-tool-contract")
                    .expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;

        let turn = session
            .send(TurnInput::text("trigger native provider tool call"))
            .output()
            .await?;

        assert_eq!(
            turn.result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::Finished {
                tool_name: "finish".to_string(),
                value: serde_json::json!(1)
            }),
            "the stray call is repaired and the next cell settles the turn"
        );
        assert!(
            !turn.result.errors.iter().any(|issue| issue.code
                == Some(crate::turn::TurnFailureCode::NativeToolCallNotAllowed.into())),
            "a repaired stray call records no terminal protocol issue: {:?}",
            turn.result.errors
        );
        let repair = repair_request
            .lock()
            .unwrap()
            .as_ref()
            .expect("a repair round issues a second provider request")
            .clone();
        assert!(repair.contains("native_lookup"), "{repair}");
        assert!(
            repair.contains("paired `<typescript>...</typescript>` block"),
            "{repair}"
        );

        let entries = trace.json_records().expect("trace records serialize");
        entries
            .iter()
            .find(|entry| {
                entry.get("type").and_then(|value| value.as_str()) == Some("protocol_step")
                    && entry.get("plugin_id").and_then(|value| value.as_str())
                        == Some(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID)
                    // The RLM event rides its format-stamped envelope (FIG-5028).
                    && entry
                        .pointer("/payload/event/RlmDiagnostic/phase")
                        .and_then(|value| value.as_str())
                        == Some("llm_extraction")
                    && entry
                        .pointer("/payload/event/RlmDiagnostic/payload/decision")
                        .and_then(|value| value.as_str())
                        == Some("retry_native_tool_call")
            })
            .unwrap_or_else(|| {
                panic!("the stray call is classified as an extraction failure: {entries:?}")
            });

        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_pending_host_tool_completion_resumes_lash_vm_await() -> Result<()> {
    run_async_test_on_stack_budget("rlm-pending-host-tool-test", || {
        rlm_pending_host_tool_completion_resumes_lash_vm_await_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_pending_host_tool_completion_resumes_lash_vm_await_inner() -> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                queued_text_provider(vec![typescript_block(
                    "const value = await tools.app_lookup({});\nawait control.finish(value);",
                )]),
                mock_llm_profile_spec(),
            )
            .tools(
                crate::tool_provider_key!("tools"),
                Arc::new(PendingAppTools::new(key_tx)),
            )
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("rlm-pending-host-tool").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let mut turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("await async app lookup"))
            .output_into(turn_events.as_ref())
            .await
    });

    let key = tokio::select! {
        key = key_rx => key.expect("pending RLM tool should send completion key"),
        result = &mut turn => panic!("RLM turn completed before issuing its completion key: {result:?}"),
    };
    let owner = SessionId::parse(session.session_id().as_str()).expect("nonblank session id");
    assert!(
        core.completions()
            .parked(crate::admin::CallOwner::Session(owner.clone()))
            .await?
            .iter()
            .any(|call| call.key == key),
        "the RLM tool's completion key is durably unresolved"
    );
    assert!(
        !turn.is_finished(),
        "RLM turn completed before external completion resolved"
    );
    assert!(
        !events
            .snapshot()
            .await
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. })),
        "pending RLM launch must not emit a completed tool result"
    );

    let payload = serde_json::json!({ "ok": true, "async": "rlm" });
    let outcome = core
        .completions()
        .resolve(key.as_str(), lash_core::Resolution::Ok(payload.clone()))
        .await?;
    assert_eq!(outcome, lash_core::ResolveAnswer::Resolved);

    let result = turn.await.expect("turn task")?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::Finished { .. })
    ));
    assert_eq!(result.finished().map(|(_, value)| value), Some(&payload));
    let events = events.snapshot().await;
    let terminal_output = events
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::Finished { value, .. } => Some(value),
            _ => None,
        })
        .expect("terminal final value");
    assert_eq!(terminal_output, &payload);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_process_pending_host_tool_completion_resumes_process_await() -> Result<()> {
    run_async_test_on_stack_budget("rlm-process-pending-host-tool-test", || {
        rlm_process_pending_host_tool_completion_resumes_process_await_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn rlm_process_pending_host_tool_completion_resumes_process_await_inner()
-> Result<()> {
    let (key_tx, key_rx) = oneshot::channel();
    let events = Arc::new(RecordingEvents::default());
    const APP_TOOLS: lash_core::ProcessConfigKey<()> =
        lash_core::ProcessConfigKey::new("pending-app-tools", lash_core::FormatVersion::ONE);
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(PendingAppTools::new(key_tx));
    let core = explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
    .serve_test_llm_profile(queued_text_provider(vec![typescript_block(
        r#"
const lookup = async () => {
    const value = await tools.app_lookup({});
    return value;
  };
const handle = await processes.start({ definition: lookup });
const result = await handle;
await control.finish(result);"#,
    )]), mock_llm_profile_spec())
    .plugin(lash_core::testing::tool_plugin(APP_TOOLS, Arc::clone(&tools)))
    .process_component(lash_core::testing::tool_component(APP_TOOLS, tools), APP_TOOLS)
    // ADR 0095: `processes` is catalogue presence, so a scripted cell that
    // authors `processes.start` needs this factory installed. Its process
    // runs under the turn's run with the app tools' component.
    .plugin(Arc::new(
        lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash_core::lifetime::session_or_starter, lash_core::testing::run_policy_naming(&[APP_TOOLS])),
    ))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("rlm-process-pending-host-tool")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let mut turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("start process with async app lookup"))
            .output_into(turn_events.as_ref())
            .await
    });

    let key = tokio::select! {
        key = key_rx => key.expect("pending process tool should send completion key"),
        result = &mut turn => panic!("process-backed turn completed before issuing its completion key: {result:?}"),
    };
    // The wait belongs to the child process, not the session, so the
    // session's outstanding read does not list it: the resolution below
    // answering `Resolved` is what shows it was durably unresolved.
    assert!(
        !turn.is_finished(),
        "process-backed turn completed before external completion resolved"
    );
    // `processes.start` is a leaf tool on the shipped surface (ADR 0095), so the
    // launch itself completes like any other call. What must not complete while
    // the external key is open is the *pending* app tool the child parked on.
    assert!(
        !events.snapshot().await.iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::ToolCallCompleted { name, .. } if name != "start_process"
        )),
        "pending process tool launch must not emit a completed tool result"
    );

    let payload = serde_json::json!({ "ok": true, "async": "process" });
    let outcome = core
        .completions()
        .resolve(key.as_str(), lash_core::Resolution::Ok(payload.clone()))
        .await?;
    assert_eq!(
        outcome,
        lash_core::ResolveAnswer::Resolved,
        "the process tool's completion key was durably unresolved"
    );

    let result = turn.await.expect("turn task")?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::Finished { .. })
    ));
    assert_eq!(result.finished().map(|(_, value)| value), Some(&payload));
    let events = events.snapshot().await;
    let terminal_output = events
        .iter()
        .find_map(|activity| match &activity.event {
            TurnEvent::Finished { value, .. } => Some(value),
            _ => None,
        })
        .expect("terminal final value");
    assert_eq!(terminal_output, &payload);
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn continue_as_observation_emits_frame_switch_then_commit() -> Result<()> {
    run_async_test_on_stack_budget("continue-as-observation-test", || {
        continue_as_observation_emits_frame_switch_then_commit_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn continue_as_observation_emits_frame_switch_then_commit_inner() -> Result<()> {
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                queued_text_provider(vec![
                    typescript_block(
                        r#"await control.continue_as({ task: "finish in a fresh frame" });"#,
                    ),
                    typescript_block(r#"await control.finish("done after continue_as");"#),
                ]),
                mock_llm_profile_spec(),
            )
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("continue-as-observation").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    // The switch answers its send; the frame's task runs next as its own
    // run (FIG-5232).
    let switched = session
        .send(TurnInput::text("switch frames"))
        .output()
        .await?;
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
        &switched.result.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.result.outcome);
    };
    let output = session
        .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
            frame_key,
        ))
        .output()
        .await?;
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!("done after continue_as"))
    );

    let resumed = session.observe().resume_from_cursor(&cursor).await?;
    let lash_core::facade_support::SessionResume::Replayed { events } = resumed else {
        panic!("recent cursor should replay continue_as observation events: {resumed:?}");
    };
    assert!(
        events.windows(2).any(|window| matches!(
            (&window[0].payload, &window[1].payload),
            (
                lash_core::SessionObservationEventPayload::AgentFrameSwitched { .. },
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        )),
        "expected AgentFrameSwitched immediately followed by Committed, got {events:?}"
    );
    Ok(())
}
