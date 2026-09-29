//! Unit tests for the standard-compaction plugin and its context-overflow
//! recovery policy.

use super::*;
use crate::recovery::*;
use lash_sansio::sync::MutexExt;
use std::sync::Mutex;

use lash_core::plugin::{PluginTraceEmitter, SessionStateService};
use lash_core::{SessionGraph, SessionPolicy};
use serde_json::json;

fn prompt_usage(used_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens: used_tokens as i64,
        ..TokenUsage::default()
    }
}

/// Mirrors what the transform and the pressure hook ask of the pressure: no
/// pressure, no decisions.
fn standard_compaction_decisions(
    usage: Option<&TokenUsage>,
    max_context_tokens: Option<usize>,
) -> (bool, bool) {
    ContextPressure::derive(usage, max_context_tokens)
        .map(|pressure| (pressure.pruning_needed(), pressure.compaction_needed()))
        .unwrap_or((false, false))
}

#[test]
fn zero_context_window_yields_no_pressure_and_no_decisions() {
    let usage = prompt_usage(130_000);
    assert_eq!(ContextPressure::derive(Some(&usage), Some(0)), None);
    assert_eq!(
        standard_compaction_decisions(Some(&usage), Some(0)),
        (false, false)
    );
}

#[test]
fn missing_usage_or_window_yields_no_decisions() {
    let usage = prompt_usage(130_000);
    assert_eq!(
        standard_compaction_decisions(None, Some(200_000)),
        (false, false)
    );
    assert_eq!(
        standard_compaction_decisions(Some(&usage), None),
        (false, false)
    );
    assert_eq!(standard_compaction_decisions(None, None), (false, false));
}

#[test]
fn non_zero_window_still_drives_both_decisions() {
    let quiet = prompt_usage(10_000);
    assert_eq!(
        standard_compaction_decisions(Some(&quiet), Some(200_000)),
        (false, false)
    );

    let pruning_only = prompt_usage(130_000);
    assert_eq!(
        standard_compaction_decisions(Some(&pruning_only), Some(200_000)),
        (true, false)
    );

    let both = prompt_usage(190_000);
    assert_eq!(
        standard_compaction_decisions(Some(&both), Some(200_000)),
        (true, true)
    );
}

fn text_message(id: &str, role: MessageRole, content: &str) -> Message {
    Message {
        id: id.to_string(),
        role,
        parts: vec![Part::text(format!("{id}.p0"), content.to_string(), None)].into(),
        origin: None,
    }
}

fn image_message(id: &str, role: MessageRole, bytes: &[u8]) -> Message {
    Message {
        id: id.to_string(),
        role,
        parts: vec![Part::attachment_part(
            format!("{id}.p0"),
            String::new(),
            Some(lash_core::session_model::message::PartAttachment {
                source: lash_core::AttachmentSource::stored(lash_core::AttachmentRef {
                    id: lash_core::AttachmentId::parse(format!("{id}-att"))
                        .expect("valid attachment id"),
                    media_type: lash_core::MediaType::parse("image/png").unwrap(),
                    byte_len: bytes.len() as u64,
                    type_metadata: None,
                    label: None,
                }),
            }),
        )]
        .into(),
        origin: None,
    }
}

use lash_core::testing::{MockSessionManager, mock_assembled_turn as empty_turn};

fn mock_manager() -> MockSessionManager {
    MockSessionManager::default()
        .with_tool_catalog(vec![
            json!({"name":"search_tools"}),
            json!({"name":"read_file"}),
        ])
        .with_turn(empty_turn(
            &SessionId::from("root"),
            "Compacted work summary",
        ))
}

/// Captures what a context hook emits through its trace-only emitter.
#[derive(Default)]
struct RecordingTraces {
    events: Mutex<Vec<(lash_core::TraceContext, lash_core::TraceEvent)>>,
}

impl RecordingTraces {
    fn emitter(self: &Arc<Self>) -> PluginTraceEmitter {
        let traces = Arc::clone(self);
        PluginTraceEmitter::new(move |context, event| {
            traces.events.lock_recover().push((context, event));
        })
    }

    fn events(&self) -> Vec<(lash_core::TraceContext, lash_core::TraceEvent)> {
        self.events.lock_recover().clone()
    }
}

fn unavailable_direct_completions() -> lash_core::facade_support::DirectCompletionClient<'static> {
    lash_core::facade_support::DirectCompletionClient::from_fn(|_, _| {
        Err(lash_core::PluginError::Session(
            "direct completions are unavailable in standard compaction tests".to_string(),
        ))
    })
}

fn test_turn_controller() -> lash_core::ScopedEffectController<'static> {
    lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::testing::UnavailableEffectController),
        lash_core::AdmittedScope::turn(SessionId::from("root"), "standard-compaction-test-turn"),
    )
    .expect("test scoped effect controller")
}

fn build_turn_ctx(
    state: SessionSnapshot,
    prompt_usage: Option<TokenUsage>,
    max_context_tokens: Option<usize>,
    traces: &Arc<RecordingTraces>,
) -> TurnTransformContext<'static> {
    TurnTransformContext {
        session_id: SessionId::from("root"),
        state: state.read_view().expect("runtime frame scope resolves"),
        prompt_usage,
        max_context_tokens,
        traces: traces.emitter(),
        scoped_effect_controller: test_turn_controller(),
        direct_completions: unavailable_direct_completions(),
    }
}

fn build_pressure_ctx(
    state: SessionSnapshot,
    prompt_usage: Option<TokenUsage>,
    max_context_tokens: Option<usize>,
    traces: &Arc<RecordingTraces>,
    direct_completions: lash_core::facade_support::DirectCompletionClient<'static>,
) -> ContextPressureContext<'static> {
    ContextPressureContext {
        session_id: SessionId::from("root"),
        state: state.read_view().expect("runtime frame scope resolves"),
        prompt_usage,
        max_context_tokens,
        traces: traces.emitter(),
        scoped_effect_controller: test_turn_controller(),
        direct_completions,
        system_prompt: None,
    }
}

fn build_compaction_ctx(
    state: SessionSnapshot,
    instructions: Option<String>,
    traces: &Arc<RecordingTraces>,
    direct_completions: lash_core::facade_support::DirectCompletionClient<'static>,
) -> CompactionContext<'static> {
    CompactionContext {
        session_id: SessionId::from("root"),
        instructions,
        state: state.read_view().expect("runtime frame scope resolves"),
        traces: traces.emitter(),
        scoped_effect_controller: lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::testing::UnavailableEffectController),
            lash_core::AdmittedScope::runtime_operation("standard-compaction-compact-test"),
        )
        .expect("test scoped effect controller"),
        direct_completions,
        system_prompt: None,
    }
}

fn llm_completion(text: &str) -> lash_core::plugin::DirectLlmCompletion {
    lash_core::plugin::DirectLlmCompletion {
        response: lash_sansio::llm::types::LlmResponse {
            parts: vec![lash_sansio::llm::types::LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            usage: Default::default(),
            terminal_reason: lash_sansio::llm::types::LlmTerminalReason::Stop,
            ..Default::default()
        },
        usage: Default::default(),
        llm_call: test_llm_call_record(),
    }
}

struct RecordingLlmCompletions {
    requests: Mutex<Vec<lash_core::LlmRequest>>,
    summary: String,
    error: Option<String>,
    terminal_reason: lash_sansio::llm::types::LlmTerminalReason,
}

impl Default for RecordingLlmCompletions {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            summary: String::new(),
            error: None,
            terminal_reason: lash_sansio::llm::types::LlmTerminalReason::Stop,
        }
    }
}

impl RecordingLlmCompletions {
    fn client(captured: &Arc<Self>) -> lash_core::facade_support::DirectCompletionClient<'static> {
        let captured = Arc::clone(captured);
        lash_core::facade_support::DirectCompletionClient::from_llm_fn(
            move |request: lash_core::LlmRequest, _source: String| {
                captured.requests.lock_recover().push(request.clone());
                if let Some(error) = &captured.error {
                    return Err(PluginError::Session(error.clone()));
                }
                let mut completion = llm_completion(&captured.summary);
                completion.response.terminal_reason = captured.terminal_reason;
                Ok(completion)
            },
        )
    }

    fn requests(&self) -> Vec<lash_core::LlmRequest> {
        self.requests.lock_recover().clone()
    }

    /// All text content the provider would see, in block order.
    fn request_text(request: &lash_core::LlmRequest) -> String {
        request
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                lash_sansio::llm::types::LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[tokio::test]
async fn standard_compaction_turn_transform_strips_old_image_attachments() {
    let messages = vec![
        image_message("u0", MessageRole::User, &[1, 2, 3]),
        text_message("u1", MessageRole::User, "recent"),
        text_message("u2", MessageRole::User, "latest"),
    ];

    let state = SessionSnapshot::new(lash_core::SessionPolicy::new(
        lash_core::TurnBudget::Unbounded,
    ));
    let traces = Arc::new(RecordingTraces::default());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let ctx = build_turn_ctx(state, Some(prompt_usage(130_000)), Some(200_000), &traces);
    let prepared = PreparedContext {
        messages: messages.into(),
        ..Default::default()
    };
    let built = transform
        .transform(&ctx, prepared)
        .await
        .expect("transform")
        .messages;

    let image_part = built[0].parts.first().expect("image part");
    assert!(matches!(image_part.kind(), PartKind::Attachment));
    assert!(image_part.attachment().is_none());
    assert_eq!(image_part.content(), PRUNED_ATTACHMENT_PLACEHOLDER);
}

/// FIG-4110: at the compaction threshold the pressure hook summarizes the
/// committed frame in one direct completion and decides a compaction frame
/// seeded with the summary. It writes nothing: core opens the frame.
#[tokio::test]
async fn pressure_hook_decides_a_summary_frame_at_the_threshold() {
    let traces = Arc::new(RecordingTraces::default());
    let direct = Arc::new(RecordingLlmCompletions {
        summary: "Compacted work summary".to_string(),
        ..Default::default()
    });
    let history = vec![
        text_message("u1", MessageRole::User, "old work"),
        text_message("a1", MessageRole::Assistant, "assistant old"),
    ];
    let ctx = build_pressure_ctx(
        compactable_state(history),
        Some(prompt_usage(30_000)),
        Some(40_000),
        &traces,
        RecordingLlmCompletions::client(&direct),
    );
    let decision = StandardCompactionPressureHook::new(StandardCompactionConfig)
        .decide(&ctx)
        .await
        .expect("the pressure hook decides");

    // One summarizer call over the committed frame.
    let requests = direct.requests();
    assert_eq!(requests.len(), 1, "exactly one direct summarizer call");
    let request_text = RecordingLlmCompletions::request_text(&requests[0]);
    assert!(request_text.contains("old work") && request_text.contains("assistant old"));

    // One compaction frame, seeded with the summary, and no record in the
    // frame it leaves.
    let ContextPressureDecision::OpenFrame {
        records,
        task,
        seed,
    } = decision
    else {
        panic!("the threshold opens a frame: {decision:?}");
    };
    assert!(
        records.is_empty(),
        "pressure records nothing in the old frame"
    );
    assert_eq!(task, "context-pressure compaction");
    let [lash_core::SessionAppendNode::Message { message: seed }] = seed.as_slice() else {
        panic!("the frame is seeded with one summary message: {seed:?}");
    };
    assert_eq!(
        seed.parts.first().map(Part::content).as_deref(),
        Some("Compaction summary:\nCompacted work summary")
    );
    assert!(matches!(
        seed.origin.as_ref(),
        Some(MessageOrigin::Plugin { plugin_id, .. }) if plugin_id == STANDARD_COMPACTION_PLUGIN_ID
    ));

    assert_eq!(
        traces
            .events()
            .into_iter()
            .map(|(context, event)| {
                assert_eq!(context.session_id.as_deref(), Some("root"));
                assert_eq!(
                    context.turn_id.as_deref(),
                    Some("standard-compaction-test-turn")
                );
                event
            })
            .collect::<Vec<_>>(),
        [lash_core::TraceEvent::CompactionNeeded {
            used_tokens: 30_000,
            max_context_tokens: 40_000,
            threshold_tokens: 20_000,
        }]
    );
}

/// Below the threshold the pressure hook continues without a summarizer call
/// or a trace.
#[tokio::test]
async fn pressure_hook_continues_below_the_threshold() {
    let traces = Arc::new(RecordingTraces::default());
    let direct = Arc::new(RecordingLlmCompletions::default());
    for usage in [None, Some(prompt_usage(10_000)), Some(prompt_usage(19_999))] {
        let ctx = build_pressure_ctx(
            compactable_state(compactable_messages()),
            usage,
            Some(40_000),
            &traces,
            RecordingLlmCompletions::client(&direct),
        );
        assert_eq!(
            StandardCompactionPressureHook::new(StandardCompactionConfig)
                .decide(&ctx)
                .await
                .expect("the pressure hook decides"),
            ContextPressureDecision::Continue
        );
    }
    assert!(direct.requests().is_empty());
    assert!(traces.events().is_empty());
}

/// Pressure over a frame with no committed conversation records the need and
/// has nothing to summarize: no summarizer call and no frame.
#[tokio::test]
async fn pressure_without_committed_history_records_the_need_and_opens_nothing() {
    let traces = Arc::new(RecordingTraces::default());
    let direct = Arc::new(RecordingLlmCompletions::default());
    let ctx = build_pressure_ctx(
        compactable_state(vec![text_message("s1", MessageRole::System, "policy")]),
        Some(prompt_usage(30_000)),
        Some(40_000),
        &traces,
        RecordingLlmCompletions::client(&direct),
    );
    let decision = StandardCompactionPressureHook::new(StandardCompactionConfig)
        .decide(&ctx)
        .await
        .expect("a frame with nothing to summarize still runs its turn");

    assert_eq!(decision, ContextPressureDecision::Continue);
    assert!(direct.requests().is_empty());
    let events = traces.events();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].1,
        lash_core::TraceEvent::CompactionNeeded { .. }
    ));
}

/// The transform is a Prompt View transform only (ADR 0001): at compaction
/// pressure it prunes the view and neither summarizes nor records the need,
/// which is the pressure hook's decision.
#[tokio::test]
async fn standard_compaction_transform_at_compaction_pressure_only_prunes() {
    let traces = Arc::new(RecordingTraces::default());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let ctx = build_turn_ctx(
        compactable_state(compactable_messages()),
        Some(prompt_usage(30_000)),
        Some(40_000),
        &traces,
    );
    let messages = vec![
        image_message("u1", MessageRole::User, b"old screenshot"),
        text_message("a1", MessageRole::Assistant, "assistant old"),
        text_message("u2", MessageRole::User, "recent"),
        text_message("u3", MessageRole::User, "latest request"),
    ];
    let built = transform
        .transform(
            &ctx,
            PreparedContext {
                messages: messages.into(),
                ..Default::default()
            },
        )
        .await
        .expect("transform")
        .messages;

    assert_eq!(
        built
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["u1", "a1", "u2", "u3"],
        "the view keeps every message"
    );
    assert_eq!(built[0].parts[0].content(), PRUNED_ATTACHMENT_PLACEHOLDER);
    assert_eq!(
        traces
            .events()
            .into_iter()
            .map(|(_, event)| event)
            .collect::<Vec<_>>(),
        [lash_core::TraceEvent::PromptViewAttachmentsPruned {
            used_tokens: 30_000,
            max_context_tokens: 40_000,
            pruned_attachments: 1,
        }]
    );
}

#[tokio::test]
async fn standard_compaction_turn_transform_traces_attachment_pruning_without_compaction() {
    // 130_000 / 200_000 trips the 0.6 pruning threshold but stays under the
    // compaction watermark: a prune-only turn still reports the prompt-view
    // change so hosts can observe why old attachments became placeholders.
    let traces = Arc::new(RecordingTraces::default());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let ctx = build_turn_ctx(state, Some(prompt_usage(130_000)), Some(200_000), &traces);
    let prepared = PreparedContext {
        // The tail since the second-most-recent user turn (a2, u3) keeps its
        // attachments; u2 and everything older is replaced by placeholders.
        messages: vec![
            image_message("u1", MessageRole::User, b"oldest screenshot"),
            image_message("u2", MessageRole::User, b"second screenshot"),
            text_message("a2", MessageRole::Assistant, "recent answer"),
            text_message("u3", MessageRole::User, "latest request"),
        ]
        .into(),
        ..Default::default()
    };

    transform
        .transform(&ctx, prepared)
        .await
        .expect("transform should trace the attachment prune");

    let events = traces.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0].1,
        lash_core::TraceEvent::PromptViewAttachmentsPruned {
            used_tokens: 130_000,
            max_context_tokens: 200_000,
            pruned_attachments: 2,
        }
    );
    assert_eq!(events[0].0.session_id.as_deref(), Some("root"));
    assert_eq!(
        events[0].0.turn_id.as_deref(),
        Some("standard-compaction-test-turn")
    );
}

#[tokio::test]
async fn standard_compaction_turn_transform_traces_nothing_when_no_attachments_pruned() {
    // Same prune-only pressure, but text-only messages: pruning finds nothing
    // to replace and must emit no event.
    let traces = Arc::new(RecordingTraces::default());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let ctx = build_turn_ctx(state, Some(prompt_usage(130_000)), Some(200_000), &traces);
    let prepared = PreparedContext {
        messages: vec![
            text_message("u1", MessageRole::User, "old work"),
            text_message("a1", MessageRole::Assistant, "assistant old"),
            text_message("u2", MessageRole::User, "latest request"),
        ]
        .into(),
        ..Default::default()
    };

    transform
        .transform(&ctx, prepared)
        .await
        .expect("transform");

    assert!(
        traces.events().is_empty(),
        "nothing was pruned: {:?}",
        traces.events()
    );
}

#[tokio::test]
async fn standard_compactor_returns_summary_seed_for_new_frame() {
    let trace = Arc::new(RecordingTraces::default());
    let messages = vec![
        text_message("u1", MessageRole::User, "old work"),
        text_message("a1", MessageRole::Assistant, "assistant old"),
        text_message("u2", MessageRole::User, "latest request"),
    ];
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        session_graph: SessionGraph::from_active_read_state(&messages),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let compaction_scope =
        lash_core::ExecutionScope::runtime_operation("standard-compaction-compact-test");
    let instructions = "focus on latest request";
    let (request_snapshot, prompt_text) =
        prepare_compaction_request(&state, messages.clone(), Some(instructions))
            .expect("prepare compaction request");
    let expected_child_ids = compaction_request_ids(
        &SessionId::from("root"),
        &state,
        &request_snapshot,
        &prompt_text,
        &compaction_scope,
    )
    .expect("derive compaction child identity");
    let (retry_snapshot, retry_prompt_text) =
        prepare_compaction_request(&state, messages.clone(), Some(instructions))
            .expect("prepare retry compaction request");
    assert_eq!(prompt_text, retry_prompt_text);
    assert_eq!(
        expected_child_ids,
        compaction_request_ids(
            &SessionId::from("root"),
            &state,
            &retry_snapshot,
            &retry_prompt_text,
            &compaction_scope,
        )
        .expect("rederive compaction child identity"),
        "retrying the same physical compaction must preserve child identity"
    );
    let (same_snapshot, changed_prompt_text) =
        prepare_compaction_request(&state, messages.clone(), Some("different focus"))
            .expect("prepare changed-prompt compaction request");
    assert_ne!(
        expected_child_ids,
        compaction_request_ids(
            &SessionId::from("root"),
            &state,
            &same_snapshot,
            &changed_prompt_text,
            &compaction_scope,
        )
        .expect("derive changed-prompt compaction child identity"),
        "different compaction prompts under one physical parent need distinct child identity"
    );
    let changed_messages = vec![
        text_message("u1", MessageRole::User, "different old work"),
        text_message("a1", MessageRole::Assistant, "assistant old"),
        text_message("u2", MessageRole::User, "latest request"),
    ];
    let mut changed_state = state.clone();
    changed_state
        .replace_active_read_state(&changed_messages)
        .expect("replace changed read state");
    let (changed_snapshot, changed_prompt_text) =
        prepare_compaction_request(&changed_state, changed_messages, Some(instructions))
            .expect("prepare changed-state compaction request");
    assert_eq!(prompt_text, changed_prompt_text);
    assert_ne!(
        expected_child_ids,
        compaction_request_ids(
            &SessionId::from("root"),
            &changed_state,
            &changed_snapshot,
            &changed_prompt_text,
            &compaction_scope,
        )
        .expect("derive changed-snapshot compaction child identity"),
        "different request snapshots under one physical parent need distinct child identity"
    );
    let captured = Arc::new(RecordingLlmCompletions {
        summary: "Compacted work summary".to_string(),
        ..Default::default()
    });
    let ctx = build_compaction_ctx(
        state,
        Some(instructions.to_string()),
        &trace,
        RecordingLlmCompletions::client(&captured),
    );
    let compactor = StandardContextCompactor::new(StandardCompactionConfig);

    let compaction = compactor
        .compact(&ctx)
        .await
        .expect("compact")
        .expect("compaction");

    assert_eq!(compaction.initial_nodes.len(), 1);
    let lash_core::SessionAppendNode::Message { message, .. } = &compaction.initial_nodes[0] else {
        panic!("expected summary message seed");
    };
    assert_eq!(message.role, MessageRole::Assistant);
    assert!(
        message
            .parts
            .first()
            .map(Part::content)
            .expect("summary text")
            .contains("Compacted work summary")
    );
    assert!(matches!(
        message.origin.as_ref(),
        Some(MessageOrigin::Plugin { plugin_id, .. }) if plugin_id == STANDARD_COMPACTION_PLUGIN_ID
    ));

    // FIG-3374: compaction is one direct completion on the calling session;
    // its context holds no lifecycle service to create one with.
    let requests = captured.requests();
    assert_eq!(requests.len(), 1, "exactly one direct provider call");
    let request = &requests[0];
    assert_eq!(request.scope.session_id, SessionId::from("root"));
    assert_eq!(request.scope.agent_frame_id, expected_child_ids.0.as_str());
    assert_eq!(request.scope.request_id, expected_child_ids.1.as_str());
    let request_text = RecordingLlmCompletions::request_text(request);
    assert!(request_text.contains("old work"));
    assert!(request_text.contains("assistant old"));
    assert!(request_text.contains("latest request"));
    assert!(request_text.contains("## Goal"));
    assert!(
        request_text.contains(instructions),
        "the focus instruction rides the summarization directive"
    );

    let events = trace.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0.session_id.as_deref(), Some("root"));
    assert_eq!(events[0].0.turn_id, None);
    assert_eq!(
        events[0].1,
        lash_core::TraceEvent::CompactionStarted {
            source_messages: 3,
            instructions_present: true,
        }
    );
    assert_eq!(
        events[1].1,
        lash_core::TraceEvent::CompactionCompleted { summary_nodes: 1 }
    );
}

#[test]
fn compaction_request_identity_is_stable_across_reconstructed_nested_maps() {
    let mut state = SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded));
    state.session_id = SessionId::from("retry-map-parent");
    for slot in [
        lash_core::PromptSlot::Intro,
        lash_core::PromptSlot::Execution,
        lash_core::PromptSlot::Guidance,
        lash_core::PromptSlot::ProjectInstructions,
        lash_core::PromptSlot::RuntimeContext,
        lash_core::PromptSlot::Environment,
    ] {
        state.policy.prompt.slots.insert(slot, Default::default());
    }

    let snapshot_value = serde_json::to_value(&state).expect("serialize snapshot");
    let expected = compaction_request_identity(&state, "same prompt")
        .expect("encode original compaction request identity");
    for _ in 0..64 {
        let reconstructed: SessionSnapshot = serde_json::from_value(snapshot_value.clone())
            .expect("reconstruct equivalent snapshot");
        assert_eq!(
            snapshot_value,
            serde_json::to_value(&reconstructed).expect("serialize reconstructed snapshot"),
            "the reconstructed snapshot must remain semantically equal"
        );
        assert_eq!(
            expected,
            compaction_request_identity(&reconstructed, "same prompt")
                .expect("encode reconstructed compaction request identity"),
            "equivalent nested prompt maps must have one canonical request identity"
        );
    }
}

#[tokio::test]
async fn standard_compactor_records_zero_node_completion_for_none() {
    let trace = Arc::new(RecordingTraces::default());
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let captured = Arc::new(RecordingLlmCompletions::default());
    let ctx = build_compaction_ctx(
        state,
        None,
        &trace,
        RecordingLlmCompletions::client(&captured),
    );

    let compaction = StandardContextCompactor::new(StandardCompactionConfig)
        .compact(&ctx)
        .await
        .expect("empty history is a successful no-op");

    assert!(compaction.is_none());
    assert_eq!(
        trace.events()[1].1,
        lash_core::TraceEvent::CompactionCompleted { summary_nodes: 0 }
    );
}

#[tokio::test]
async fn standard_compactor_records_zero_node_completion_before_error() {
    let trace = Arc::new(RecordingTraces::default());
    let messages = vec![
        text_message("u1", MessageRole::User, "old work"),
        text_message("a1", MessageRole::Assistant, "assistant old"),
        text_message("u2", MessageRole::User, "latest request"),
    ];
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        session_graph: SessionGraph::from_active_read_state(&messages),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let captured = Arc::new(RecordingLlmCompletions {
        error: Some("scripted compaction-session failure".to_string()),
        ..Default::default()
    });
    let ctx = build_compaction_ctx(
        state,
        None,
        &trace,
        RecordingLlmCompletions::client(&captured),
    );

    let error = StandardContextCompactor::new(StandardCompactionConfig)
        .compact(&ctx)
        .await
        .expect_err("scripted completion failure must propagate");

    assert!(
        error
            .to_string()
            .contains("scripted compaction-session failure")
    );
    assert_eq!(
        trace.events()[1].1,
        lash_core::TraceEvent::CompactionCompleted { summary_nodes: 0 }
    );
}

// ---- context-overflow recovery (FIG-2950) ----

fn overflow_turn_report(
    outcome: lash_core::facade_support::TurnOutcome,
) -> Arc<lash_core::plugin::TurnHookReport> {
    let mut turn = empty_turn(&SessionId::from("root"), "overflow");
    turn.outcome = outcome;
    Arc::new(lash_core::plugin::TurnHookReport::from_assembled(&turn))
}

fn recovery_ctx(
    state: SessionSnapshot,
    direct: &Arc<RecordingLlmCompletions>,
    traces: &Arc<RecordingTraces>,
    max_context_tokens: usize,
) -> ContextPressureContext<'static> {
    ContextPressureContext {
        session_id: SessionId::from("root"),
        state: state.read_view().expect("runtime frame scope resolves"),
        prompt_usage: None,
        max_context_tokens: Some(max_context_tokens),
        traces: traces.emitter(),
        scoped_effect_controller: lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::testing::UnavailableEffectController),
            lash_core::AdmittedScope::runtime_operation("standard-compaction-recovery-test"),
        )
        .expect("test scoped effect controller"),
        direct_completions: RecordingLlmCompletions::client(direct),
        system_prompt: None,
    }
}

async fn decide_recovery(ctx: &ContextPressureContext<'_>) -> ContextPressureDecision {
    StandardCompactionPressureHook::new(StandardCompactionConfig)
        .decide(ctx)
        .await
        .expect("a recovery decision never fails the turn")
}

/// The recovery record kinds a decision's nodes carry, in order.
fn decided_record_kinds(nodes: &[lash_core::SessionAppendNode]) -> Vec<OverflowRecoveryRecord> {
    nodes
        .iter()
        .map(|node| {
            let lash_core::SessionAppendNode::Message { message } = node else {
                panic!("a recovery record is a message node: {node:?}");
            };
            recovery_record_kind(&Message {
                id: "probe".to_string(),
                role: message.role,
                parts: message.parts.clone().into(),
                origin: message.origin.clone(),
            })
            .expect("a decided recovery record parses")
            .expect("the node is a recovery record")
        })
        .collect()
}

fn test_llm_call_record() -> lash_core::LlmCallRecord {
    lash_core::LlmCallRecord {
        call_id: lash_core::LlmCallId("recovery-test-call".to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts: Vec::new(),
    }
}

/// A summarizer that always answers with the recovered summary text.
fn recovered_direct() -> Arc<RecordingLlmCompletions> {
    Arc::new(RecordingLlmCompletions {
        summary: "Recovered: the user asked for the report verdict; it is done.".to_string(),
        ..Default::default()
    })
}

/// A summarizer that always returns an empty completion — the
/// `insufficient_reduction` failure path.
fn empty_direct() -> Arc<RecordingLlmCompletions> {
    Arc::new(RecordingLlmCompletions::default())
}

fn recovery_record_node_message(record: OverflowRecoveryRecord) -> Message {
    let plugin_message = recovery_record_message(record);
    Message {
        id: "m_recover_record".into(),
        role: MessageRole::System,
        parts: plugin_message.parts.into(),
        origin: plugin_message.origin,
    }
}

fn recovery_history(big_part: bool) -> (Vec<Message>, SessionSnapshot) {
    let oversized = "x".repeat(OVERFLOW_RECOVERY_ELIDE_PART_THRESHOLD_TOKENS * 4);
    let mut messages = vec![
        text_message("s1", MessageRole::System, "session policy"),
        text_message("u1", MessageRole::User, "summarize the attached report"),
        text_message("t1", MessageRole::User, &oversized),
    ];
    if big_part {
        messages.push(recovery_record_node_message(
            OverflowRecoveryRecord::Pending,
        ));
    }
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        session_graph: SessionGraph::from_active_read_state(&messages),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    (messages, state)
}

/// The test mutates its durable history between drives so each attempt
/// reads the record the previous one appended, like a restore would. The
/// still-open pending marker survives; terminal records are replaced by
/// the next attempt's result.
fn history_with_record(messages: &mut Vec<Message>, record: OverflowRecoveryRecord) {
    messages.retain(|message| {
        !matches!(
            recovery_record_kind(message),
            Ok(Some(
                OverflowRecoveryRecord::Failed { .. }
                    | OverflowRecoveryRecord::Completed
                    | OverflowRecoveryRecord::Exhausted
            ))
        )
    });
    messages.push(recovery_record_node_message(record));
}

#[tokio::test]
async fn overflow_after_turn_queues_marker_for_context_overflow_outcome_only() {
    let manager = Arc::new(mock_manager());
    let sessions: Arc<dyn SessionStateService> = manager.clone();

    let overflow = lash_core::plugin::TurnResultHookContext {
        session_id: SessionId::from("root"),
        turn: overflow_turn_report(lash_core::facade_support::TurnOutcome::Stopped(
            lash_core::facade_support::TurnStop::ContextOverflow,
        )),
        sessions: sessions.clone(),
    };
    let directives = overflow_recovery_after_turn(&overflow)
        .await
        .expect("hook runs");
    assert_eq!(directives.len(), 2);
    let marker = match &directives[1] {
        lash_core::plugin::AfterTurnPluginDirective::EnqueueMessages(directive) => {
            &directive.messages[0]
        }
        other => panic!("expected enqueue directive, got {other:?}"),
    };
    assert!(
        marker
            .parts
            .first()
            .map(Part::content)
            .unwrap()
            .starts_with(OVERFLOW_RECOVERY_MARKER)
    );

    // The control row: a plain provider error names no recovery trigger.
    let provider_error = lash_core::plugin::TurnResultHookContext {
        session_id: SessionId::from("root"),
        turn: overflow_turn_report(lash_core::facade_support::TurnOutcome::Stopped(
            lash_core::facade_support::TurnStop::ProviderError,
        )),
        sessions,
    };
    assert!(
        overflow_recovery_after_turn(&provider_error)
            .await
            .expect("hook runs")
            .is_empty()
    );

    // Ignored handoff guidance: a cooperative agent-frame switch outcome
    // is not an overflow, so no marker is queued.
    let frame_key = lash_core::FrameKey::from_caller_material("continue-as").expect("non-empty");
    let guided = lash_core::plugin::TurnResultHookContext {
        session_id: SessionId::from("root"),
        turn: overflow_turn_report(lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
            frame_key,
            task: "continue the task".to_string(),
            initial_nodes: Vec::new(),
        }),
        sessions: Arc::new(MockSessionManager::default()),
    };
    assert!(
        overflow_recovery_after_turn(&guided)
            .await
            .expect("hook runs")
            .is_empty(),
        "cooperative handoff guidance must not open a recovery"
    );
}

#[test]
fn recovery_state_derivation_is_bounded_and_durable() {
    let pending = || vec![OverflowRecoveryRecord::Pending];
    assert_eq!(
        OverflowRecoveryState::derive(pending()),
        OverflowRecoveryState::Pending { attempts: 0 }
    );

    let failing = OverflowRecoveryState::derive(vec![
        OverflowRecoveryRecord::Pending,
        OverflowRecoveryRecord::Failed { attempt: 2 },
    ]);
    assert_eq!(failing, OverflowRecoveryState::Pending { attempts: 2 });
    assert!(!failing.exhausted());

    let exhausted = OverflowRecoveryState::derive(vec![
        OverflowRecoveryRecord::Pending,
        OverflowRecoveryRecord::Failed { attempt: 2 },
        OverflowRecoveryRecord::Failed { attempt: 3 },
    ]);
    assert_eq!(
        exhausted,
        OverflowRecoveryState::Pending {
            attempts: OVERFLOW_RECOVERY_MAX_ATTEMPTS
        }
    );
    assert!(exhausted.exhausted());

    let done = OverflowRecoveryState::derive(vec![
        OverflowRecoveryRecord::Pending,
        OverflowRecoveryRecord::Failed { attempt: 1 },
        OverflowRecoveryRecord::Completed,
    ]);
    assert_eq!(done, OverflowRecoveryState::Idle);
}

/// The prose title only marks a message as a recovery record; the record is
/// always read back from its serde payload. A corrupt payload is an error —
/// never an `Exhausted` guess from the title.
#[test]
fn recovery_record_reads_its_serde_payload_only() {
    let record_message = |text: String| Message {
        id: "m".to_string(),
        role: MessageRole::System,
        parts: vec![Part::text("m.p0".to_string(), text, None)].into(),
        origin: Some(MessageOrigin::Plugin {
            plugin_id: STANDARD_COMPACTION_PLUGIN_ID.to_string(),
            transient: false,
        }),
    };

    let written = recovery_record_node_message(OverflowRecoveryRecord::Failed { attempt: 2 });
    assert_eq!(
        recovery_record_kind(&written).expect("written record parses"),
        Some(OverflowRecoveryRecord::Failed { attempt: 2 })
    );

    for payload in ["{not json", "{\"kind\":\"bogus\"}", ""] {
        let corrupt = record_message(format!("{OVERFLOW_RECOVERY_FAILED}\n{payload}"));
        assert!(
            recovery_record_kind(&corrupt).is_err(),
            "malformed payload {payload:?} must error, not guess a record"
        );
    }

    // A title with another record's payload is read as the payload's kind.
    let relabeled = record_message(format!(
        "{OVERFLOW_RECOVERY_EXHAUSTED}\n{{\"kind\":\"pending\"}}"
    ));
    assert_eq!(
        recovery_record_kind(&relabeled).expect("payload deserializes"),
        Some(OverflowRecoveryRecord::Pending)
    );
}

/// FIG-4110: a pending overflow recovery summarizes the committed history
/// (the oversized part elided) in one direct completion and decides the
/// recovery frame: the `Completed` record in the frame it leaves, the summary
/// as the new frame's seed.
#[tokio::test]
async fn recovery_runs_unasked_elides_oversized_result_and_decides_a_recovery_frame() {
    let traces = Arc::new(RecordingTraces::default());
    let (_, state) = recovery_history(true);
    let direct = recovered_direct();
    let ctx = recovery_ctx(state, &direct, &traces, 200_000);

    let decision = decide_recovery(&ctx).await;

    // One direct completion ran as the summarizer: the provider-visible
    // request carries the rendered history plus the standard compaction ask
    // and the recovery instructions, never the oversized body.
    let requests = direct.requests();
    assert_eq!(requests.len(), 1, "exactly one direct summarizer call");
    let request = &requests[0];
    assert_eq!(request.scope.session_id, SessionId::from("root"));
    assert!(
        request.scope.request_id.contains("standard-compaction:"),
        "the replay key keeps the compaction attempt identity: {:?}",
        request.scope.request_id
    );
    let request_text = RecordingLlmCompletions::request_text(request);
    assert!(
        request_text.contains("##") && request_text.contains("the conversation above"),
        "the recovery summarizer runs the standard compaction prompt: {}",
        request_text
    );
    assert!(
        request_text.contains(OVERFLOW_RECOVERY_INSTRUCTIONS),
        "the recovery summarizer must carry the recovery instructions"
    );
    assert!(
        request_text.len() < 40_000,
        "the summarizer request itself must fit its window: {}",
        request_text.len()
    );
    assert!(
        request_text.contains(OVERFLOW_ELIDED_PART_PLACEHOLDER),
        "the oversized part was elided before the summarizer saw the history"
    );

    let ContextPressureDecision::OpenFrame {
        records,
        task,
        seed,
    } = decision
    else {
        panic!("a completed recovery opens its frame: {decision:?}");
    };
    assert_eq!(
        decided_record_kinds(&records),
        [OverflowRecoveryRecord::Completed],
        "the completed record closes the recovery in the frame it leaves"
    );
    assert_eq!(task, "context-overflow recovery");
    let [lash_core::SessionAppendNode::Message { message: seed }] = seed.as_slice() else {
        panic!("the recovery frame is seeded with one summary: {seed:?}");
    };
    let seed_text = seed.parts.first().map(Part::content).unwrap_or_default();
    assert!(
        seed_text.starts_with("Compaction summary:") && seed_text.contains("Recovered"),
        "the recovery frame is seeded with the recovered summary: {seed_text:?}"
    );
    assert!(
        !seed_text.contains("xxxxxxxxxxxxxxxxxxxxxxxxxx"),
        "the oversized body never enters the recovery frame"
    );
}

fn snapshot_with_messages(messages: &[Message]) -> SessionSnapshot {
    SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        session_graph: SessionGraph::from_active_read_state(messages),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    }
}

/// FIG-3107 regression, on the FIG-3374 seam: the summarizer request must not
/// carry the plugin's own still-open recovery marker. A request that still
/// derived `pending` would let a future reader of that transcript re-run the
/// very recovery it is summarizing. Before the original fix this recursed
/// without bound through nested `-compaction:` sessions; with the direct
/// completion the same guarantee is pinned on the provider-visible request.
#[tokio::test]
async fn recovery_summarizer_request_does_not_carry_the_pending_marker() {
    let traces = Arc::new(RecordingTraces::default());
    let (_history, state) = recovery_history(true);
    let direct = recovered_direct();
    decide_recovery(&recovery_ctx(state, &direct, &traces, 200_000)).await;

    let requests = direct.requests();
    assert_eq!(requests.len(), 1, "exactly one summarizer call ran");
    let request_text = RecordingLlmCompletions::request_text(&requests[0]);
    assert!(
        !request_text.contains(OVERFLOW_RECOVERY_MARKER.trim_end_matches(':')),
        "the summarizer request must not carry the pending recovery marker it is recovering from"
    );
}

/// Every failed attempt records `Failed`; the attempt that hits the cap also
/// records `Exhausted`; none opens a frame. At the cap the recovery stops by
/// itself: no summarizer call, no record, no loop.
#[tokio::test]
async fn recovery_failure_is_bounded_and_explicit() {
    let empty = empty_direct();
    let (mut history, _) = recovery_history(true);

    for attempt in 1..=OVERFLOW_RECOVERY_MAX_ATTEMPTS {
        let traces = Arc::new(RecordingTraces::default());
        let decision = decide_recovery(&recovery_ctx(
            snapshot_with_messages(&history),
            &empty,
            &traces,
            200_000,
        ))
        .await;
        let ContextPressureDecision::Record { nodes } = decision else {
            panic!("attempt {attempt} records its failure and opens no frame: {decision:?}");
        };
        let mut expected = vec![OverflowRecoveryRecord::Failed {
            attempt: attempt as u32,
        }];
        if attempt == OVERFLOW_RECOVERY_MAX_ATTEMPTS {
            expected.push(OverflowRecoveryRecord::Exhausted);
        }
        assert_eq!(decided_record_kinds(&nodes), expected, "attempt {attempt}");

        history_with_record(
            &mut history,
            OverflowRecoveryRecord::Failed {
                attempt: attempt as u32,
            },
        );
    }
    assert_eq!(empty.requests().len(), OVERFLOW_RECOVERY_MAX_ATTEMPTS);

    let captured = Arc::new(RecordingLlmCompletions::default());
    let traces = Arc::new(RecordingTraces::default());
    assert_eq!(
        decide_recovery(&recovery_ctx(
            snapshot_with_messages(&history),
            &captured,
            &traces,
            200_000,
        ))
        .await,
        ContextPressureDecision::Continue,
        "the cap spends no further attempt"
    );
    assert!(captured.requests().is_empty());

    let outcomes: Vec<String> = traces
        .events()
        .into_iter()
        .filter_map(|(_, event)| match event {
            lash_core::TraceEvent::Custom { name, payload }
                if name == TRACE_OVERFLOW_RECOVERY_OUTCOME =>
            {
                payload
                    .get("outcome")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            }
            _ => None,
        })
        .collect();
    assert_eq!(outcomes, ["exhausted:recoverable_failure"]);
}

/// A summarizer that fails outright records the attempt as `Failed`, names
/// the failure in its trace, and opens no frame.
#[tokio::test]
async fn recovery_summarizer_failure_records_failed_without_a_frame() {
    let failing = Arc::new(RecordingLlmCompletions {
        error: Some("scripted summarizer failure".to_string()),
        ..Default::default()
    });
    let traces = Arc::new(RecordingTraces::default());
    let (_, state) = recovery_history(true);
    let decision = decide_recovery(&recovery_ctx(state, &failing, &traces, 200_000)).await;

    let ContextPressureDecision::Record { nodes } = decision else {
        panic!("a failed summarizer records its attempt and opens no frame: {decision:?}");
    };
    assert_eq!(
        decided_record_kinds(&nodes),
        [OverflowRecoveryRecord::Failed { attempt: 1 }]
    );
    assert_eq!(failing.requests().len(), 1);
    assert!(
        traces.events().iter().any(|(_, event)| matches!(
            event,
            lash_core::TraceEvent::Custom { name, payload }
                if name == TRACE_OVERFLOW_RECOVERY_OUTCOME
                    && payload.get("outcome").and_then(|value| value.as_str()).is_some_and(
                        |outcome| outcome.starts_with("failed:summarizer_failed")
                            && outcome.contains("scripted summarizer failure")
                    )
        )),
        "{:?}",
        traces.events()
    );
}

#[tokio::test]
async fn recovery_does_not_restart_after_completion_or_exhaustion() {
    for terminal in [
        OverflowRecoveryRecord::Completed,
        OverflowRecoveryRecord::Exhausted,
    ] {
        let traces = Arc::new(RecordingTraces::default());
        let captured = Arc::new(RecordingLlmCompletions::default());
        let (mut messages, _) = recovery_history(false);
        messages.push(recovery_record_node_message(
            OverflowRecoveryRecord::Pending,
        ));
        messages.push(recovery_record_node_message(terminal));

        assert_eq!(
            decide_recovery(&recovery_ctx(
                snapshot_with_messages(&messages),
                &captured,
                &traces,
                200_000,
            ))
            .await,
            ContextPressureDecision::Continue,
            "a settled recovery must not reopen: {terminal:?}"
        );
        assert!(captured.requests().is_empty());
    }
}

fn compactable_state(messages: Vec<Message>) -> SessionSnapshot {
    SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        session_graph: SessionGraph::from_active_read_state(&messages),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    }
}

fn compactable_messages() -> Vec<Message> {
    vec![
        text_message("u1", MessageRole::User, "old work"),
        text_message("a1", MessageRole::Assistant, "assistant old"),
        text_message("u2", MessageRole::User, "latest request"),
    ]
}

#[tokio::test]
async fn compaction_request_carries_the_core_resolved_system_prompt() {
    let captured = Arc::new(RecordingLlmCompletions {
        summary: "summary".to_string(),
        ..Default::default()
    });
    let mut ctx = build_compaction_ctx(
        compactable_state(compactable_messages()),
        None,
        &Arc::new(RecordingTraces::default()),
        RecordingLlmCompletions::client(&captured),
    );
    ctx.system_prompt = Some(Arc::from("resolved capability+core+session stack"));
    StandardContextCompactor::new(StandardCompactionConfig)
        .compact(&ctx)
        .await
        .expect("compact")
        .expect("compaction");
    let requests = captured.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].instructions.as_deref(),
        Some("resolved capability+core+session stack"),
        "the request carries the prompt the core resolved, not a plugin-side rebuild"
    );
}

#[tokio::test]
async fn standard_compactor_refuses_incomplete_terminal_reasons_as_frame_seed() {
    for reason in [
        lash_sansio::llm::types::LlmTerminalReason::OutputLimit,
        lash_sansio::llm::types::LlmTerminalReason::ContentFilter,
        lash_sansio::llm::types::LlmTerminalReason::ToolUse,
        lash_sansio::llm::types::LlmTerminalReason::Cancelled,
    ] {
        let captured = Arc::new(RecordingLlmCompletions {
            summary: "partial summary".to_string(),
            terminal_reason: reason,
            ..Default::default()
        });
        let ctx = build_compaction_ctx(
            compactable_state(compactable_messages()),
            None,
            &Arc::new(RecordingTraces::default()),
            RecordingLlmCompletions::client(&captured),
        );
        let err = StandardContextCompactor::new(StandardCompactionConfig)
            .compact(&ctx)
            .await
            .expect_err("an incomplete completion must not seed a durable frame");
        assert!(
            err.to_string().contains(reason.code()),
            "error names the terminal reason: {err}"
        );
    }
}

#[test]
fn summary_recognition_requires_standard_compaction_origin() {
    let mut summary = text_message("summary", MessageRole::Assistant, COMPACTION_SUMMARY_TITLE);
    summary.origin = Some(MessageOrigin::Plugin {
        plugin_id: "standard_compaction".into(),
        transient: false,
    });
    assert!(is_compaction_summary_message(&summary));
    summary.origin = Some(MessageOrigin::Plugin {
        plugin_id: "other_plugin".into(),
        transient: false,
    });
    assert!(!is_compaction_summary_message(&summary));
}
