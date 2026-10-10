//! What a standard turn settles, through a host's `send()` on a served
//! node (FIG-5310, ported from the deleted laws of
//! lash-core's `runtime/tests/persistence.rs` and
//! `runtime/tests/attachment_continuation.rs`).
//!
//! Each law scripts the model's calls, sends one input and reads the turn's
//! settled report, its activity and the session's committed head.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::TurnEvent;
use lash_core::llm::types::{
    LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse, LlmStreamEvent, LlmUsage,
    StreamBlockIdentity,
};
use lash_core::testing::runtime_helpers::{EchoTool, MockCall, TerminalControlTool, mock_provider};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, World};

/// A core on `tier` whose model answers `calls` in order, with `tools`.
async fn world(
    tier: Tier,
    calls: Vec<MockCall>,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
) -> Option<World> {
    World::with_model(
        tier,
        Vec::new(),
        mock_provider(calls).into_handle(),
        move |backend| {
            let builder = lash::LashCore::standard_builder(backend.clone());
            match tools {
                Some(tools) => builder.tools(tools),
                None => builder,
            }
        },
    )
    .await
}

fn text(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_owned(),
        response_meta: None,
    }
}

fn delta(text: &str) -> LlmStreamEvent {
    LlmStreamEvent::Block(StreamBlockEvent::Delta {
        kind: StreamBlockKind::AssistantText,
        block: StreamBlockIdentity::new("text:0", 0),
        text: text.to_owned(),
    })
}

fn usage(input_tokens: i64, output_tokens: i64, cache_read_input_tokens: i64) -> LlmUsage {
    LlmUsage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    }
}

fn answer(parts: Vec<LlmOutputPart>) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts,
            ..LlmResponse::default()
        }),
    }
}

fn tool_call(call_id: &str, tool: &str, input: serde_json::Value) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: call_id.to_owned(),
        tool_name: tool.to_owned(),
        input_json: input.to_string(),
        replay: None,
    }
}

/// The assistant prose the turn streamed to the host, joined.
fn streamed_prose(output: &lash::TurnOutput) -> String {
    output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text,
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A model whose final response is empty settles the prose it streamed:
/// the reply is the streamed text, committed as one assistant message, and
/// the host saw each delta once.
async fn standard_runtime_recovers_streamed_text_when_final_response_is_empty(tier: Tier) {
    let expected =
        "I’m continuing with a type-safety cleanup now: replace the remaining raw JSON paths.";
    let Some(world) = world(
        tier,
        vec![MockCall {
            stream_events: vec![
                delta("I’m continuing with a type-safety cleanup now: "),
                delta("replace the remaining raw JSON paths."),
            ],
            response: Ok(LlmResponse::default()),
        }],
        None,
    )
    .await
    else {
        return;
    };
    let session = world
        .session("recover-streamed-text", served::spec(8))
        .await;
    let output = world.send(&session, "continue").await;

    served::assert_answered("the streamed turn", &output);
    assert_eq!(output.assistant_message(), Some(expected));
    assert!(
        output.result.errors.is_empty(),
        "{:?}",
        output.result.errors
    );
    let view = output.result.state.read_view();
    let replies = view
        .messages()
        .iter()
        .filter(|message| message.role == lash_core::MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1, "one assistant message: {replies:?}");
    assert_eq!(replies[0].parts[0].content(), expected);
    assert_eq!(streamed_prose(&output), expected);
    world.shutdown().await;
}

/// A text part the stream delivers after the deltas that spelled it
/// reconciles with them: the reply holds the sentence once, and the host
/// saw it streamed once.
async fn standard_runtime_text_part_reconciles_without_streaming_duplicate(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![MockCall {
            stream_events: vec![
                delta("The sentence."),
                LlmStreamEvent::Part(text("The sentence.")),
            ],
            response: Ok(LlmResponse::default()),
        }],
        None,
    )
    .await
    else {
        return;
    };
    let session = world
        .session("text-part-no-duplicate", served::spec(8))
        .await;
    let output = world.send(&session, "continue").await;

    assert_eq!(
        output.result.assistant_message().unwrap_or_default(),
        "The sentence."
    );
    assert_eq!(streamed_prose(&output), "The sentence.");
    world.shutdown().await;
}

/// A tool that answers `control.finish`'s control without declaring it.
struct UndeclaredFinish;

impl UndeclaredFinish {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:smuggle_finish",
            "smuggle_finish",
            "Answers with a turn control it does not declare.",
            serde_json::json!({ "type": "object" }),
            serde_json::json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(30))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for UndeclaredFinish {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "smuggle_finish").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::finish(lash_core::ToolValue::untrusted_json(serde_json::json!(
            "smuggled"
        )))
        .into()
    }
}

/// Law 10 (FIG-5781): a tool whose result is a turn control it did not
/// declare is refused at settlement with a typed failure, and the turn does
/// not end on it: the model reads the failure and answers.
async fn a_tool_answering_an_undeclared_control_is_refused_and_the_turn_goes_on(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![
            answer(vec![tool_call(
                "tool-1",
                "smuggle_finish",
                serde_json::json!({}),
            )]),
            answer(vec![text("answered after the refusal")]),
        ],
        Some(Arc::new(UndeclaredFinish)),
    )
    .await
    else {
        return;
    };
    let session = world.session("undeclared-control", served::spec(8)).await;
    let output = world.send(&session, "smuggle a finish").await;

    assert_eq!(output.finished(), None, "{:?}", output.result.outcome);
    assert_eq!(output.result.tool_calls.len(), 1);
    assert!(
        matches!(
            &output.result.tool_calls[0].output.outcome,
            lash_core::ToolCallOutcome::Failure(failure)
                if matches!(
                    failure.cause.as_deref(),
                    Some(lash_core::ToolFailureCause::Declaration {
                        refusal: lash_core::DeclarationRefusal::UndeclaredControl {
                            control: lash_core::TurnControlKind::Finish,
                        },
                    })
                )
        ),
        "the undeclared control is refused: {:?}",
        output.result.tool_calls[0]
    );
    assert!(
        matches!(
            &output.result.outcome,
            lash_core::facade_support::TurnOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage { text }
            )
                if text == "answered after the refusal"
        ),
        "{:?}",
        output.result.outcome
    );
    world.shutdown().await;
}

/// A tool that finishes the turn with its value ends it with the first such
/// value in declared order; the second control is refused, and every call completes before the
/// turn's value is published, and the committed reply is that value,
/// marked as the turn's one reply.
async fn standard_runtime_tool_control_finish_emits_terminal_output(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![
            answer(vec![
                tool_call("tool-1", "terminal_tool_0", serde_json::json!({})),
                tool_call("tool-2", "terminal_tool_1", serde_json::json!({})),
            ]),
            answer(vec![text("unexpected follow-up")]),
        ],
        Some(Arc::new(TerminalControlTool {
            controls: vec![
                lash_core::ToolControl::Turn {
                    control: lash_core::TurnControl::Finish {
                        value: lash_core::ToolValue::untrusted_json(serde_json::json!("first")),
                        value_schema: None,
                    },
                },
                lash_core::ToolControl::Turn {
                    control: lash_core::TurnControl::Finish {
                        value: lash_core::ToolValue::untrusted_json(serde_json::json!("second")),
                        value_schema: None,
                    },
                },
            ],
        })),
    )
    .await
    else {
        return;
    };
    let session = world.session("terminal-tool-finish", served::spec(8)).await;
    let output = world.send(&session, "run terminal tools").await;

    assert_eq!(
        output.finished(),
        Some(("terminal_tool_0", &serde_json::json!("first"))),
        "outcome={:?} calls={:?}",
        output.result.outcome,
        output.result.tool_calls
    );
    assert_eq!(output.result.tool_calls.len(), 2);
    // A refused call is reported before the step's executed calls, so the
    // second control call is found by its tool.
    let second_call = output
        .result
        .tool_calls
        .iter()
        .find(|call| call.tool == "terminal_tool_1")
        .expect("the second control call is recorded");
    assert!(
        matches!(
            &second_call.output.outcome,
            lash_core::ToolCallOutcome::Failure(failure)
                if matches!(failure.cause.as_deref(), Some(lash_core::ToolFailureCause::ControlAttemptSpent))
        ),
        "the second control call is refused: {second_call:?}"
    );

    let position = |matches: &dyn Fn(&TurnEvent) -> bool| {
        output
            .activities
            .iter()
            .position(|activity| matches(&activity.event))
    };
    let first = position(&|event| {
        matches!(event, TurnEvent::ToolCallCompleted { name, .. } if name == "terminal_tool_0")
    })
    .expect("the first call completed");
    let second = position(&|event| {
        matches!(event, TurnEvent::ToolCallCompleted { name, .. } if name == "terminal_tool_1")
    })
    .expect("the second call completed");
    let terminal = position(&|event| matches!(event, TurnEvent::Finished { .. }))
        .expect("the turn's value was published");
    assert!(
        first < terminal && second < terminal,
        "{:?}",
        output.activities
    );
    assert!(matches!(
        &output.activities[terminal].event,
        TurnEvent::Finished { tool_name, value }
            if tool_name == "terminal_tool_0" && *value == serde_json::json!("first")
    ));
    let view = output.result.state.read_view();
    let replies = view
        .messages()
        .iter()
        .filter(|message| message.reply_marker.is_some())
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1, "one reply: {:?}", view.messages());
    let marker = replies[0].reply_marker.as_ref().expect("marked");
    let part = replies[0]
        .parts
        .iter()
        .find(|part| part.id() == marker.part_id())
        .expect("the marker names a part of its message");
    assert_eq!(part.content(), "first");
    world.shutdown().await;
}

/// A tool that fails the turn stops it with the tool's typed failure and
/// publishes no final or tool value.
async fn standard_runtime_tool_control_fail_stops_without_terminal_output_event(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![
            answer(vec![tool_call(
                "tool-1",
                "terminal_tool_0",
                serde_json::json!({}),
            )]),
            answer(vec![text("unexpected follow-up")]),
        ],
        Some(Arc::new(TerminalControlTool {
            controls: vec![lash_core::ToolControl::Fail {
                failure: lash_core::ToolFailure::tool(
                    lash_core::ToolFailureClass::Execution,
                    "terminal_control_failed",
                    "failed",
                ),
            }],
        })),
    )
    .await
    else {
        return;
    };
    let session = world.session("terminal-tool-fail", served::spec(8)).await;
    let output = world.send(&session, "run failing terminal tool").await;

    assert!(
        matches!(
            &output.result.outcome,
            lash_core::facade_support::TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ToolError {
                tool_name,
                value,
            }) if tool_name == "terminal_tool_0"
                && value["code"] == "terminal_control_failed"
                && value["message"] == "failed"
        ),
        "outcome={:?} calls={:?}",
        output.result.outcome,
        output.result.tool_calls
    );
    assert!(
        !output
            .activities
            .iter()
            .any(|activity| matches!(activity.event, TurnEvent::Finished { .. })),
        "{:?}",
        output.activities
    );
    world.shutdown().await;
}

/// A tool call the model streamed as a part, with an empty final response,
/// runs: its result pairs with the provider's call id, and the next call
/// answers the turn.
async fn standard_runtime_executes_streamed_tool_call_when_final_response_is_empty(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![
            MockCall {
                stream_events: vec![
                    LlmStreamEvent::Part(tool_call(
                        "tool-1",
                        "echo_tool",
                        serde_json::json!({"value": "sample"}),
                    )),
                    LlmStreamEvent::Usage(usage(12, 3, 0)),
                ],
                response: Ok(LlmResponse::default()),
            },
            answer(vec![text("done")]),
        ],
        Some(Arc::new(EchoTool)),
    )
    .await
    else {
        return;
    };
    let session = world.session("streamed-tool-call", served::spec(8)).await;
    let output = world.send(&session, "run the tool").await;

    assert_eq!(
        output.result.assistant_message().unwrap_or_default(),
        "done"
    );
    let calls = &output.result.tool_calls;
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].provider_call_id.as_deref(), Some("tool-1"));
    assert_eq!(
        calls[0].output.value_for_projection(),
        serde_json::json!({ "payload": "raw:sample" })
    );
    world.shutdown().await;
}

/// An unstreamed response's text parts keep their boundaries: the reply
/// joins them as paragraphs, the committed message keeps one part each, and
/// the host sees each part as a stream block of its own (Lash injects no
/// separator into a block; merging adjacent blocks is the host's choice).
async fn standard_runtime_preserves_part_boundaries_when_response_is_not_streamed(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![answer(vec![text("Intro paragraph."), text("## Heading")])],
        None,
    )
    .await
    else {
        return;
    };
    let session = world.session("part-boundaries", served::spec(8)).await;
    let output = world.send(&session, "hi").await;

    assert_eq!(
        output.result.assistant_message().unwrap_or_default(),
        "Intro paragraph.\n\n## Heading"
    );
    let blocks = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::StreamBlock(StreamBlockEvent::Completed { block, text, .. }) => {
                Some((block.id.clone(), text.to_string()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        blocks,
        [
            ("part:0".to_owned(), "Intro paragraph.".to_owned()),
            ("part:1".to_owned(), "## Heading".to_owned()),
        ]
    );
    assert_eq!(streamed_prose(&output), "Intro paragraph.## Heading");
    let view = output.result.state.read_view();
    let reply = view
        .messages()
        .iter()
        .rfind(|message| message.role == lash_core::MessageRole::Assistant)
        .expect("the reply is committed");
    assert_eq!(
        reply
            .parts
            .iter()
            .map(|part| part.content().into_owned())
            .collect::<Vec<_>>(),
        ["Intro paragraph.", "\n\n## Heading"]
    );
    world.shutdown().await;
}

/// The usage the turn's activity and its sealed call record report: the
/// turn's cumulative `Usage` and the call's one attempt's. A durable report
/// carries no usage of its own (`ReportSource::Durable`): hosts meter at the
/// provider seam (ADR 0127) and read the turn's from its activity.
fn reported_usage(output: &lash::TurnOutput) -> Vec<(i64, i64, i64)> {
    let triple = |usage: &LlmUsage| {
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_input_tokens,
        )
    };
    let cumulative = output
        .activities
        .iter()
        .rev()
        .find_map(|activity| match &activity.event {
            TurnEvent::Usage { cumulative, .. } => Some((
                cumulative.input_tokens,
                cumulative.output_tokens,
                cumulative.cache_read_input_tokens,
            )),
            _ => None,
        })
        .expect("the turn reported its usage");
    let attempts = output
        .result
        .llm_calls
        .iter()
        .flat_map(|call| call.attempts.iter())
        .map(|attempt| {
            attempt
                .usage
                .as_ref()
                .map(triple)
                .expect("the attempt's usage")
        })
        .collect::<Vec<_>>();
    std::iter::once(cumulative).chain(attempts).collect()
}

/// A response that reports no usage of its own is billed what its stream
/// reported.
async fn standard_runtime_uses_streamed_usage_when_final_usage_missing(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![MockCall {
            stream_events: vec![delta("Hi"), LlmStreamEvent::Usage(usage(9, 3, 2))],
            response: Ok(LlmResponse {
                parts: vec![text("Hi")],
                usage: LlmUsage::default(),
                ..LlmResponse::default()
            }),
        }],
        None,
    )
    .await
    else {
        return;
    };
    let session = world.session("streamed-usage", served::spec(8)).await;
    let output = world.send(&session, "hello").await;

    assert_eq!(reported_usage(&output), [(9, 3, 2), (9, 3, 2)]);
    world.shutdown().await;
}

/// A response that reports its own usage is billed that, not what its
/// stream reported.
async fn standard_runtime_prefers_final_usage_over_streamed_usage(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![MockCall {
            stream_events: vec![delta("Hi"), LlmStreamEvent::Usage(usage(9, 3, 2))],
            response: Ok(LlmResponse {
                parts: vec![text("Hi")],
                usage: usage(12, 4, 1),
                ..LlmResponse::default()
            }),
        }],
        None,
    )
    .await
    else {
        return;
    };
    let session = world.session("final-usage", served::spec(8)).await;
    let output = world.send(&session, "hello").await;

    assert_eq!(reported_usage(&output), [(12, 4, 1), (12, 4, 1)]);
    world.shutdown().await;
}

/// A turn whose second model call overflows the turn's cumulative usage
/// counter refuses without a settled report: no turn commits, and the
/// session's head publishes neither the overflowing answer nor usage.
async fn cumulative_usage_overflow_publishes_no_turn_terminal_or_usage(tier: Tier) {
    let Some(world) = world(
        tier,
        vec![
            MockCall {
                stream_events: vec![LlmStreamEvent::Usage(usage(i64::MAX - 1, 0, 0))],
                response: Ok(LlmResponse {
                    parts: vec![tool_call(
                        "overflow-tool-call",
                        "echo_tool",
                        serde_json::json!({"value": "continue"}),
                    )],
                    ..LlmResponse::default()
                }),
            },
            MockCall {
                stream_events: vec![LlmStreamEvent::Usage(usage(2, 0, 0))],
                response: Ok(LlmResponse {
                    parts: vec![text("must not commit")],
                    ..LlmResponse::default()
                }),
            },
        ],
        Some(Arc::new(EchoTool)),
    )
    .await
    else {
        return;
    };
    let session = world.session("usage-overflow", served::spec(8)).await;
    let error = tokio::time::timeout(
        served::WATCHDOG,
        session
            .send(lash::TurnInput::text("use the tool, then answer"))
            .output(),
    )
    .await
    .expect("deadlock watchdog: the overflowing turn settles")
    .expect_err("overflow refuses the run instead of publishing a turn report");
    let lash::EmbedError::Runtime(refusal) = error else {
        panic!("expected the runtime's overflow refusal, got {error:?}");
    };
    assert_eq!(refusal.code, lash_core::RuntimeErrorCode::StoreRefused);
    assert_eq!(
        refusal.message,
        "token usage counter `input_tokens` overflowed while accumulating (turn, tool-semantics-model)"
    );
    let view = session.read().await.expect("the session reads");
    assert_eq!(
        view.as_ref()
            .expect("the created session has a head")
            .token_usage(),
        &lash_core::LlmUsage::default(),
        "the overflowing turn publishes no durable usage"
    );
    let turns = session
        .committed_turns(None, std::num::NonZeroU32::new(1).unwrap())
        .await
        .expect("read committed turns");
    assert!(
        turns.turns.is_empty(),
        "no turn terminal commits: {turns:?}"
    );
    let committed = view
        .as_ref()
        .map(|view| view.messages().to_vec())
        .unwrap_or_default();
    assert!(
        !committed
            .iter()
            .flat_map(|message| message.parts.iter())
            .any(|part| part.content().contains("must not commit")),
        "the overflowing call's answer is never committed: {committed:?}"
    );
    world.shutdown().await;
}

const UNSUPPORTED_BYTES: &[u8] = b"native workspace badge binary bytes";

/// Returns one stored attachment of its media type.
struct AttachmentResultTool {
    media_type: &'static str,
    bytes: &'static [u8],
    label: &'static str,
}

fn attachment_result_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:attachment_result",
        "attachment_result",
        "Return one stored attachment.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for AttachmentResultTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![attachment_result_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "attachment_result")
            .then(|| Arc::new(attachment_result_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let attachment_ref = call
            .context
            .attachments()
            .put(
                self.bytes.to_vec(),
                lash_core::AttachmentCreateMeta::new(
                    lash_core::MediaType::parse(self.media_type).expect("test MIME"),
                    None,
                    Some(self.label.to_owned()),
                ),
            )
            .await
            .expect("store tool attachment");
        lash_core::ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(
            lash_core::ToolValue::Attachment(attachment_ref),
        ))
        .into()
    }
}

/// Every call the attachment model answered: the request it was asked and
/// the wire its provider was handed, its attachment slots filled from the
/// session's store for that attempt.
#[derive(Default)]
struct Asked {
    calls: Mutex<Vec<(LlmRequest, serde_json::Value)>>,
}

impl Asked {
    fn requests(&self) -> Vec<LlmRequest> {
        self.calls
            .lock_recover()
            .iter()
            .map(|(request, _)| request.clone())
            .collect()
    }

    fn bodies(&self) -> Vec<serde_json::Value> {
        self.calls
            .lock_recover()
            .iter()
            .map(|(_, body)| body.clone())
            .collect()
    }
}

/// Calls `attachment_result` first, then answers each request; a request
/// carrying an attachment no acceptor takes is refused as a provider would.
/// It lowers canonically, so its wire holds each attachment where the
/// request names it, filled by the canonical codec from the delivery the
/// session's store made for the attempt.
fn attachment_model(asked: Arc<Asked>) -> lash_core::facade_support::ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    lash_core::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete_with_wire(move |request: LlmRequest, wire: String| {
            let asked = Arc::clone(&asked);
            let calls = Arc::clone(&calls);
            async move {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                let lowered = serde_json::from_str(&wire).unwrap_or(serde_json::Value::Null);
                asked.calls.lock_recover().push((request.clone(), lowered));
                if index == 0 {
                    return Ok(LlmResponse {
                        parts: vec![tool_call(
                            "attachment-result-call",
                            "attachment_result",
                            serde_json::json!({}),
                        )],
                        ..LlmResponse::default()
                    });
                }
                let position =
                    lash_sansio::llm::attachment_delivery::AttachmentPosition::ToolResult;
                if let Some(reference) = request.attachments().find(|reference| {
                    lash_core::llm::transport::known_attachment_acceptors(
                        &request.attachment_acceptance,
                        reference,
                        position,
                    )
                    .is_empty()
                }) {
                    return Err(
                        lash_core::llm::transport::unsupported_attachment_capability(
                            "OpenAI Chat Completions",
                            reference,
                            position,
                            &[],
                        ),
                    );
                }
                Ok(LlmResponse {
                    parts: vec![text(&format!("completed provider call {index}"))],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// Every value the canonical slot codec filled into `wire`, in order.
fn slot_values(wire: &serde_json::Value) -> Vec<serde_json::Value> {
    match wire {
        serde_json::Value::Object(object)
            if object.len() == 1
                && ["bytes_base64", "url", "provider_file"]
                    .iter()
                    .any(|form| object.contains_key(*form)) =>
        {
            vec![wire.clone()]
        }
        serde_json::Value::Object(object) => object.values().flat_map(slot_values).collect(),
        serde_json::Value::Array(values) => values.iter().flat_map(slot_values).collect(),
        _ => Vec::new(),
    }
}

fn request_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.to_string()),
            LlmContentBlock::ToolResult { content, .. } => {
                Some(lash_core::facade_support::tool_result_text(content).into_owned())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The session spec of the attachment laws: the host catalogue takes PNG
/// images from the scenario's provider, as bytes, in tool results and
/// messages, and no other media.
fn attachment_spec() -> lash::SessionSpec {
    use lash_sansio::llm::attachment_delivery::{AttachmentPosition, DeliveryForms};
    use lash_sansio::llm::capability::{
        AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
    };
    served::spec(8).attachment_acceptance(Arc::new(AttachmentCapabilitySnapshot {
        revision: "standard-turn-outcomes".to_owned(),
        acceptors: vec![AttachmentAcceptor {
            provider: "mock".to_owned(),
            rules: vec![AttachmentAcceptanceRule {
                positions: vec![AttachmentPosition::Message, AttachmentPosition::ToolResult],
                media_types: vec!["image/png".to_owned()],
                media_families: Vec::new(),
                forms: DeliveryForms {
                    bytes: true,
                    url: false,
                    provider_file: false,
                },
            }],
        }],
    }))
}

/// A committed tool attachment no provider accepts degrades instead of
/// failing the session: the tool's result records a typed
/// `attachment_unavailable` notice naming it when the result is admitted,
/// every later request omits its bytes and carries that notice, and the
/// turn after it still answers.
async fn unsupported_committed_tool_attachment_degrades_and_session_remains_continuable(
    tier: Tier,
) {
    let asked = Arc::new(Asked::default());
    let Some(world) = World::with_model(
        tier,
        Vec::new(),
        attachment_model(Arc::clone(&asked)),
        |backend| {
            lash::LashCore::standard_builder(backend.clone()).tools(Arc::new(
                AttachmentResultTool {
                    media_type: "application/octet-stream",
                    bytes: UNSUPPORTED_BYTES,
                    label: "workspace_badge.bin",
                },
            ))
        },
    )
    .await
    else {
        return;
    };
    let session = world
        .session("unsupported-attachment", attachment_spec())
        .await;
    let artifact = world.send(&session, "fetch the workspace badge").await;
    let follow_up = world
        .send(&session, "answer this text-only follow-up")
        .await;

    assert_eq!(artifact.result.tool_calls.len(), 1);
    assert!(artifact.result.tool_calls[0].output.is_success());
    assert!(
        artifact.is_success() && follow_up.is_success(),
        "an unmaterializable attachment and its history stay continuable: {:?} {:?}; {:?} {:?}",
        artifact.result.outcome,
        artifact.result.errors,
        follow_up.result.outcome,
        follow_up.result.errors
    );
    let requests = asked.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(
            request.attachments().next().is_none(),
            "an unmaterializable attachment is omitted from provider requests"
        );
        let notice = request_text(request)
            .split("<attachment_unavailable>")
            .nth(1)
            .and_then(|tail| tail.split("</attachment_unavailable>").next())
            .map(|payload| {
                serde_json::from_str::<serde_json::Value>(payload).expect("a typed notice")
            })
            .unwrap_or_else(|| panic!("a typed notice in {}", request_text(request)));
        assert_eq!(notice["label"], "workspace_badge.bin");
        assert_eq!(notice["media_type"], "application/octet-stream");
        assert_eq!(notice["position"], "tool_result");
        assert_eq!(notice["reason"], "no_provider_accepts_mime_and_position");
    }
    world.shutdown().await;
}

/// A tool attachment the provider accepts reaches the next request as a
/// ref, and the provider's wire as the stored bytes the session's store
/// delivered for that attempt into the call's slot, with no degradation
/// note (WIRE-SLOTS through the runtime's attempt loop).
async fn accepted_tool_attachment_round_trips_without_degradation(tier: Tier) {
    const IMAGE_BYTES: &[u8] = b"accepted-image-bytes";
    let asked = Arc::new(Asked::default());
    let Some(world) = World::with_model(
        tier,
        Vec::new(),
        attachment_model(Arc::clone(&asked)),
        |backend| {
            lash::LashCore::standard_builder(backend.clone()).tools(Arc::new(
                AttachmentResultTool {
                    media_type: "image/png",
                    bytes: IMAGE_BYTES,
                    label: "accepted.png",
                },
            ))
        },
    )
    .await
    else {
        return;
    };
    let session = world
        .session("accepted-attachment", attachment_spec())
        .await;
    let output = world.send(&session, "fetch the accepted image").await;

    served::assert_answered("the accepted attachment's turn", &output);
    let requests = asked.requests();
    assert_eq!(requests.len(), 2);
    let replay = &requests[1];
    let attachments = replay.attachments().collect::<Vec<_>>();
    assert_eq!(attachments.len(), 1);
    let attachment_ref = attachments[0];
    assert_eq!(attachment_ref.media_type.as_str(), "image/png");
    assert_eq!(attachment_ref.label.as_deref(), Some("accepted.png"));
    // base64 of `IMAGE_BYTES`, the canonical codec's bytes form.
    let delivered = serde_json::json!({ "bytes_base64": "YWNjZXB0ZWQtaW1hZ2UtYnl0ZXM=" });
    assert_eq!(
        slot_values(&asked.bodies()[1]),
        [delivered],
        "the provider's wire carries the attachment's stored bytes in its one slot"
    );
    assert!(!request_text(replay).contains("attachment_unavailable"));
    world.shutdown().await;
}

/// Returns `["before", <stored image>, "after"]`: an array tool value that
/// embeds an attachment between two text fragments.
struct ArrayAttachmentTool;

fn array_attachment_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:array_attachment",
        "array_attachment",
        "Return an array embedding one stored image.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "array" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ArrayAttachmentTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![array_attachment_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "array_attachment")
            .then(|| Arc::new(array_attachment_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let attachment_ref = call
            .context
            .attachments()
            .put(
                b"array-image".to_vec(),
                lash_core::AttachmentCreateMeta::new(
                    lash_core::MediaType::parse("image/png").expect("test MIME"),
                    None,
                    Some("array.png".to_owned()),
                ),
            )
            .await
            .expect("store tool attachment");
        lash_core::ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(
            lash_core::ToolValue::Array(vec![
                lash_core::ToolValue::String("before".to_owned()),
                lash_core::ToolValue::Attachment(attachment_ref),
                lash_core::ToolValue::String("after".to_owned()),
            ]),
        ))
        .into()
    }
}

/// FIG-3515: a tool value embedding an attachment commits one resume-safe
/// result for its call, with the image in its place. An `Immediate` cancel
/// of the next turn, while its model answers that turn's executed call,
/// leaves nothing of it half-committed: a cancelled turn ends with no head
/// commit, so the head is still the first turn's whole result, resume-safe,
/// and the session's next turn answers over it.
async fn attachment_in_array_tool_value_then_immediate_cancel_loses_nothing(tier: Tier) {
    let (answering_tx, answering_rx) = tokio::sync::oneshot::channel::<()>();
    let answering_tx = Arc::new(Mutex::new(Some(answering_tx)));
    let calls = Arc::new(AtomicUsize::new(0));
    let model = lash_core::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request: LlmRequest| {
            let calls = Arc::clone(&calls);
            let answering_tx = Arc::clone(&answering_tx);
            async move {
                let array_call = |call_id: &str| LlmResponse {
                    parts: vec![tool_call(
                        call_id,
                        "array_attachment",
                        serde_json::json!({}),
                    )],
                    ..LlmResponse::default()
                };
                let answer = |text_: &str| LlmResponse {
                    parts: vec![text(text_)],
                    ..LlmResponse::default()
                };
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(array_call("turn-one-call")),
                    1 => Ok(answer("turn one done")),
                    2 => Ok(array_call("turn-two-call")),
                    _ => {
                        // Turn 2's call has executed; hold the model's answer
                        // open until the host cancels the turn. Every later
                        // call answers.
                        let held = answering_tx.lock_recover().take();
                        match held {
                            Some(tx) => {
                                let _ = tx.send(());
                                std::future::pending::<
                                    Result<
                                        LlmResponse,
                                        lash_core::llm::transport::LlmTransportError,
                                    >,
                                >()
                                .await
                            }
                            None => Ok(answer("turn three done")),
                        }
                    }
                }
            }
        })
        .build()
        .into_handle();
    let Some(world) = World::with_model(tier, Vec::new(), model, |backend| {
        lash::LashCore::standard_builder(backend.clone()).tools(Arc::new(ArrayAttachmentTool))
    })
    .await
    else {
        return;
    };
    let session = world
        .session("array-attachment-immediate-cancel", attachment_spec())
        .await;
    let turn_one = world.send(&session, "return the array").await;
    served::assert_answered("the array turn", &turn_one);
    assert!(
        lash_sansio::messages_are_prompt_resume_safe(turn_one.result.state.read_view().messages()),
        "an attachment-bearing tool value commits a resume-safe transcript"
    );

    let handle = session
        .send(lash::TurnInput::text("turn two input"))
        .await
        .expect("the second input is accepted");
    tokio::time::timeout(served::WATCHDOG, answering_rx)
        .await
        .expect("deadlock watchdog: the model answers turn 2's executed call")
        .expect("the model is answering turn 2's executed call");
    handle
        .cancel()
        .mode(lash_core::TurnCancelMode::Immediate)
        .reason("user stopped the turn")
        .await
        .expect("the cancel is accepted");
    let turn_two = tokio::time::timeout(served::WATCHDOG, handle.output())
        .await
        .expect("deadlock watchdog: the cancelled turn settles")
        .expect("the cancelled turn answers");
    assert_eq!(
        turn_two.status(),
        lash::TurnStatus::Cancelled,
        "{:?}",
        turn_two.result.outcome
    );

    let head = session
        .read()
        .await
        .expect("the session reads")
        .expect("a committed head");
    let parts = head
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .collect::<Vec<_>>();
    assert!(
        !parts.iter().any(|part| part.content() == "turn two input"
            || part.provider_call_id() == Some("turn-two-call")),
        "a cancelled turn commits nothing of itself: {parts:?}"
    );
    let turn_one_call = parts
        .iter()
        .find(|part| part.provider_call_id() == Some("turn-one-call"))
        .and_then(|part| part.call_id())
        .cloned()
        .expect("turn 1's call is committed");
    let results = parts
        .iter()
        .filter(|part| {
            part.kind() == lash_core::PartKind::ToolResult && part.call_id() == Some(&turn_one_call)
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "turn 1's call has one result");
    let blocks = results[0]
        .tool_result_content()
        .expect("tool result blocks");
    let images = blocks
        .iter()
        .filter_map(|block| block.attachment())
        .map(|attachment| attachment.label.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        images,
        [Some("array.png".to_owned())],
        "the image stays the result's one attachment: {blocks:?}"
    );
    assert!(lash_sansio::messages_are_prompt_resume_safe(
        head.messages()
    ));

    let turn_three = world.send(&session, "turn three input").await;
    served::assert_answered("the turn after the cancel", &turn_three);
    assert!(lash_sansio::messages_are_prompt_resume_safe(
        turn_three.result.state.read_view().messages()
    ));
    world.shutdown().await;
}

/// FIG-5800: an `Immediate` cancel the host commits while a control call's
/// finish is decided at the completion checkpoint, before the turn's commit
/// fences cancellation, wins. The run ends cancelled, and the finish the
/// checkpoint accepted never reaches the host: its `Finished` is held for
/// the commit the cancel prevented, and discarded with it.
async fn an_immediate_cancel_before_an_accepted_finish_commits_publishes_no_finished(tier: Tier) {
    const RUN: &str = "cancelled-finish-run";
    let core: Arc<std::sync::OnceLock<lash::LashCore>> = Arc::default();
    let cancels = Arc::new(AtomicUsize::new(0));
    let hook_core = Arc::clone(&core);
    let hook_cancels = Arc::clone(&cancels);
    let cancel_at_completion: Arc<dyn lash_core::plugin::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("cancel-at-completion"),
            lash_core::facade_support::PluginSpec::new().with_checkpoint(
                lash_core::hook_key!("cancel-at-completion"),
                Arc::new(move |ctx: lash_core::plugin::CheckpointHookContext| {
                    let core = Arc::clone(&hook_core);
                    let cancels = Arc::clone(&hook_cancels);
                    Box::pin(async move {
                        if ctx.checkpoint == lash_core::CheckpointKind::BeforeCompletion
                            && cancels.fetch_add(1, Ordering::SeqCst) == 0
                        {
                            let core = core.get().expect("the core is built before its turn");
                            core.session(ctx.session_id.clone())
                                .durable()
                                .await
                                .expect("the session's durable handle")
                                .cancel(lash::CancelTarget::Run(
                                    lash::TurnId::parse(RUN).expect("a run id"),
                                ))
                                .mode(lash_core::TurnCancelMode::Immediate)
                                .reason("the host stopped the finishing turn")
                                .await
                                .expect("the cancel is accepted");
                        }
                        Ok(Default::default())
                    })
                }),
            ),
        ));
    let Some(world) = World::with_model(
        tier,
        Vec::new(),
        mock_provider(vec![
            answer(vec![tool_call(
                "finish-1",
                "terminal_tool_0",
                serde_json::json!({}),
            )]),
            answer(vec![text("unexpected follow-up")]),
        ])
        .into_handle(),
        move |backend| {
            lash::LashCore::standard_builder(backend.clone())
                .tools(Arc::new(TerminalControlTool {
                    controls: vec![lash_core::ToolControl::Turn {
                        control: lash_core::TurnControl::Finish {
                            value: lash_core::ToolValue::untrusted_json(serde_json::json!(
                                "accepted"
                            )),
                            value_schema: None,
                        },
                    }],
                }))
                .plugin(cancel_at_completion)
        },
    )
    .await
    else {
        return;
    };
    assert!(core.set(world.core.clone()).is_ok(), "the core is set once");
    let session = world
        .session("cancelled-accepted-finish", served::spec(8))
        .await;
    let output = tokio::time::timeout(
        served::WATCHDOG,
        session
            .send(lash::TurnInput::text("finish, then be cancelled"))
            .id(lash::TurnId::parse(RUN).expect("a run id"))
            .output(),
    )
    .await
    .expect("deadlock watchdog: the cancelled turn settles")
    .expect("the cancelled turn answers");

    assert_eq!(
        cancels.load(Ordering::SeqCst),
        1,
        "the cancel was requested once"
    );
    assert_eq!(
        output.status(),
        lash::TurnStatus::Cancelled,
        "{:?}",
        output.result.outcome
    );
    let published_finish = output
        .activities
        .iter()
        .filter(|activity| matches!(activity.event, TurnEvent::Finished { .. }))
        .collect::<Vec<_>>();
    assert!(
        published_finish.is_empty(),
        "a cancelled turn published a finish: {published_finish:?}"
    );
    world.shutdown().await;
}

/// A session handle opened before the node ran its turn closes without a
/// write: an open builds no capabilities, so its park reads the head the
/// node committed through its own store and adopts it rather than flushing
/// its stale state over it (FIG-5310).
async fn a_session_opened_before_its_served_turn_closes_without_a_write(tier: Tier) {
    let Some(world) = world(tier, vec![answer(vec![text("served")])], None).await else {
        return;
    };
    let session = world
        .session("close-after-served-turn", served::spec(8))
        .await;
    let live = world
        .core
        .session(session.session_id().clone())
        .open()
        .await
        .expect("open the session before its turn");
    let output = tokio::time::timeout(
        served::WATCHDOG,
        live.send(lash::TurnInput::text("one served turn")).output(),
    )
    .await
    .expect("deadlock watchdog: the turn settles")
    .expect("the turn answers");
    served::assert_answered("the served turn", &output);
    let head = || async {
        world
            .backend
            .session_store_factory()
            .load_session_head_meta(session.session_id())
            .await
            .expect("read the head")
            .expect("the session has a head")
            .head_revision
    };
    let before = head().await;
    live.close()
        .await
        .expect("a handle whose turn the node ran closes cleanly");
    assert_eq!(head().await, before, "the close wrote nothing");
    world.shutdown().await;
}

tiered_laws!(
    standard_runtime_recovers_streamed_text_when_final_response_is_empty,
    standard_runtime_text_part_reconciles_without_streaming_duplicate,
    standard_runtime_tool_control_finish_emits_terminal_output,
    a_tool_answering_an_undeclared_control_is_refused_and_the_turn_goes_on,
    standard_runtime_tool_control_fail_stops_without_terminal_output_event,
    standard_runtime_executes_streamed_tool_call_when_final_response_is_empty,
    standard_runtime_preserves_part_boundaries_when_response_is_not_streamed,
    standard_runtime_uses_streamed_usage_when_final_usage_missing,
    standard_runtime_prefers_final_usage_over_streamed_usage,
    cumulative_usage_overflow_publishes_no_turn_terminal_or_usage,
    unsupported_committed_tool_attachment_degrades_and_session_remains_continuable,
    accepted_tool_attachment_round_trips_without_degradation,
    attachment_in_array_tool_value_then_immediate_cancel_loses_nothing,
    an_immediate_cancel_before_an_accepted_finish_commits_publishes_no_finished,
    a_session_opened_before_its_served_turn_closes_without_a_write,
);
