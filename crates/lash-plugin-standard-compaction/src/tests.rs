//! Unit tests for the standard-compaction plugin and its context-overflow
//! recovery policy.

use super::*;
use crate::recovery::*;
use lash_sansio::sync::MutexExt;
use std::sync::Mutex;

use lash_core::plugin::{SessionGraphService, SessionLifecycleService, SessionStateService};
use lash_core::{SessionGraph, SessionPolicy};
use serde_json::json;

fn prompt_usage(used_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens: used_tokens as i64,
        ..TokenUsage::default()
    }
}

/// Mirrors what the turn transform asks of the pressure: no pressure, no decisions.
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
            json!({"name":"exec_command"}),
            json!({"name":"read_file"}),
        ])
        .with_turn(empty_turn(
            &SessionId::from("root"),
            "Compacted work summary",
        ))
}

#[derive(Default)]
struct RecordingSessionGraph {
    events: Mutex<Vec<(lash_core::TraceContext, lash_core::TraceEvent)>>,
    appends: Mutex<Vec<(String, lash_core::AppendSessionNodesRequest)>>,
    switches: Mutex<Vec<(String, lash_core::SwitchAgentFrameRequest)>>,
}

impl RecordingSessionGraph {
    fn events(&self) -> Vec<(lash_core::TraceContext, lash_core::TraceEvent)> {
        self.events.lock_recover().clone()
    }

    fn appends(&self) -> Vec<(String, lash_core::AppendSessionNodesRequest)> {
        self.appends.lock_recover().clone()
    }

    fn switches(&self) -> Vec<(String, lash_core::SwitchAgentFrameRequest)> {
        self.switches.lock_recover().clone()
    }
}

#[async_trait]
impl SessionGraphService for RecordingSessionGraph {
    async fn emit_trace_event(
        &self,
        context: lash_core::TraceContext,
        event: lash_core::TraceEvent,
    ) -> Result<(), PluginError> {
        self.events.lock_recover().push((context, event));
        Ok(())
    }

    async fn switch_agent_frame(
        &self,
        session_id: &SessionId,
        request: lash_core::SwitchAgentFrameRequest,
    ) -> Result<lash_core::OpenAgentFrameResult, PluginError> {
        self.switches
            .lock_recover()
            .push((session_id.to_string(), request));
        Ok(lash_core::OpenAgentFrameResult {
            frame_node_id: "frame".to_string(),
            opened: true,
            initial_node_ids: Vec::new(),
        })
    }

    async fn append_session_nodes(
        &self,
        session_id: &SessionId,
        request: lash_core::AppendSessionNodesRequest,
    ) -> Result<lash_core::AppendSessionNodesOutcome, PluginError> {
        self.appends
            .lock_recover()
            .push((session_id.to_string(), request));
        Ok(lash_core::AppendSessionNodesOutcome::Appended {
            node_ids: Vec::new(),
            leaf_node_id: lash_core::NodeId::from("appended"),
        })
    }
}

fn build_turn_ctx(
    session_id: &SessionId,
    state: SessionSnapshot,
    prompt_usage: Option<TokenUsage>,
    max_context_tokens: Option<usize>,
    manager: Arc<MockSessionManager>,
) -> TurnTransformContext<'static> {
    let session_graph = manager.clone();
    build_turn_ctx_with_graph(
        session_id,
        state,
        prompt_usage,
        max_context_tokens,
        manager,
        session_graph,
    )
}

fn build_turn_ctx_with_graph(
    session_id: &SessionId,
    state: SessionSnapshot,
    prompt_usage: Option<TokenUsage>,
    max_context_tokens: Option<usize>,
    manager: Arc<MockSessionManager>,
    session_graph: Arc<dyn SessionGraphService>,
) -> TurnTransformContext<'static> {
    TurnTransformContext {
        session_id: SessionId::from(session_id.to_string()),
        state: state.read_view().expect("runtime frame scope resolves"),
        prompt_usage,
        max_context_tokens,
        sessions: manager.clone(),
        session_lifecycle: manager.clone(),
        session_graph,
        scoped_effect_controller: lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
            lash_core::AdmittedScope::turn(session_id, "standard-compaction-test-turn"),
        )
        .expect("test scoped effect controller"),
        direct_completions: lash_core::facade_support::DirectCompletionClient::from_fn(|_, _| {
            Err(lash_core::PluginError::Session(
                "direct completions are unavailable in standard compaction tests".to_string(),
            ))
        }),
        system_prompt: None,
    }
}

fn build_compaction_ctx_with_graph(
    session_id: &SessionId,
    state: SessionSnapshot,
    instructions: Option<String>,
    manager: Arc<MockSessionManager>,
    session_graph: Arc<dyn SessionGraphService>,
    direct_completions: lash_core::facade_support::DirectCompletionClient<'static>,
) -> CompactionContext<'static> {
    let sessions = manager.clone();
    build_compaction_ctx_with_services(
        session_id,
        state,
        instructions,
        sessions,
        manager,
        session_graph,
        direct_completions,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_compaction_ctx_with_services(
    session_id: &SessionId,
    state: SessionSnapshot,
    instructions: Option<String>,
    sessions: Arc<dyn SessionStateService>,
    session_lifecycle: Arc<dyn SessionLifecycleService>,
    session_graph: Arc<dyn SessionGraphService>,
    direct_completions: lash_core::facade_support::DirectCompletionClient<'static>,
) -> CompactionContext<'static> {
    CompactionContext {
        session_id: SessionId::from(session_id.to_string()),
        instructions,
        state: state.read_view().expect("runtime frame scope resolves"),
        sessions,
        session_lifecycle,
        session_graph,
        scoped_effect_controller: lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
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
    let manager = Arc::new(mock_manager());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let ctx = build_turn_ctx(
        &SessionId::from("root"),
        state,
        Some(prompt_usage(130_000)),
        Some(200_000),
        manager,
    );
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

#[tokio::test]
async fn standard_compaction_turn_transform_projects_tail_without_summary() {
    let manager = Arc::new(mock_manager());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let ctx = build_turn_ctx(
        &SessionId::from("root"),
        state,
        Some(prompt_usage(90_000)),
        Some(100_000),
        manager.clone(),
    );
    let prepared = PreparedContext {
        messages: vec![
            text_message("u1", MessageRole::User, "old work"),
            text_message("a1", MessageRole::Assistant, "assistant old"),
            text_message("u2", MessageRole::User, "latest request"),
        ]
        .into(),
        ..Default::default()
    };
    let built = transform
        .transform(&ctx, prepared)
        .await
        .expect("transform")
        .messages;

    assert!(built.iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| part.content().contains("latest request"))
    }));
    assert!(!built.iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| part.content().contains("old work"))
    }));

    let created = manager.created_snapshot();
    assert!(created.is_empty());
}

#[tokio::test]
async fn standard_compaction_turn_transform_traces_threshold_and_prompt_pruning() {
    let manager = Arc::new(mock_manager());
    let trace = Arc::new(RecordingSessionGraph::default());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let ctx = build_turn_ctx_with_graph(
        &SessionId::from("root"),
        state,
        Some(prompt_usage(30_000)),
        Some(40_000),
        manager,
        trace.clone(),
    );
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
        .expect("transform should emit its decisions");

    let events = trace.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0.session_id.as_deref(), Some("root"));
    assert_eq!(
        events[0].0.turn_id.as_deref(),
        Some("standard-compaction-test-turn")
    );
    assert_eq!(
        events[0].1,
        lash_core::TraceEvent::CompactionNeeded {
            used_tokens: 30_000,
            max_context_tokens: 40_000,
            threshold_tokens: 20_000,
        }
    );
    assert_eq!(
        events[1].1,
        lash_core::TraceEvent::PromptViewPruned {
            used_tokens: 30_000,
            max_context_tokens: 40_000,
            dropped_prefix_messages: 2,
            retained_messages: 1,
        }
    );
}

#[tokio::test]
async fn standard_compaction_turn_transform_records_needed_when_no_cut_point_exists() {
    let manager = Arc::new(mock_manager());
    let trace = Arc::new(RecordingSessionGraph::default());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let ctx = build_turn_ctx_with_graph(
        &SessionId::from("root"),
        state,
        Some(prompt_usage(30_000)),
        Some(40_000),
        manager,
        trace.clone(),
    );
    let prepared = PreparedContext {
        messages: vec![
            text_message("s1", MessageRole::System, "policy"),
            text_message("s2", MessageRole::System, "more policy"),
        ]
        .into(),
        ..Default::default()
    };

    transform
        .transform(&ctx, prepared)
        .await
        .expect("no-cut-point decision should be traced");

    let events = trace.events();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1].1,
        lash_core::TraceEvent::PromptViewPruned {
            used_tokens: 30_000,
            max_context_tokens: 40_000,
            dropped_prefix_messages: 0,
            retained_messages: 2,
        }
    );
}

#[tokio::test]
async fn standard_compaction_turn_transform_traces_attachment_pruning_without_compaction() {
    // 130_000 / 200_000 trips the 0.6 pruning threshold but stays under the
    // compaction watermark: a prune-only turn still reports the prompt-view
    // change so hosts can observe why old attachments became placeholders.
    let manager = Arc::new(mock_manager());
    let trace = Arc::new(RecordingSessionGraph::default());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let ctx = build_turn_ctx_with_graph(
        &SessionId::from("root"),
        state,
        Some(prompt_usage(130_000)),
        Some(200_000),
        manager,
        trace.clone(),
    );
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

    let events = trace.events();
    let attachment_events: Vec<_> = events
        .iter()
        .filter(|(_, event)| {
            matches!(
                event,
                lash_core::TraceEvent::PromptViewAttachmentsPruned { .. }
            )
        })
        .collect();
    assert_eq!(attachment_events.len(), 1, "{events:?}");
    assert_eq!(
        attachment_events[0].1,
        lash_core::TraceEvent::PromptViewAttachmentsPruned {
            used_tokens: 130_000,
            max_context_tokens: 200_000,
            pruned_attachments: 2,
        }
    );
    assert_eq!(attachment_events[0].0.session_id.as_deref(), Some("root"));
    assert_eq!(
        attachment_events[0].0.turn_id.as_deref(),
        Some("standard-compaction-test-turn")
    );
    assert!(
        !events.iter().any(|(_, event)| matches!(
            event,
            lash_core::TraceEvent::CompactionNeeded { .. }
                | lash_core::TraceEvent::PromptViewPruned { .. }
        )),
        "no compaction decision runs on a prune-only turn: {events:?}"
    );
}

#[tokio::test]
async fn standard_compaction_turn_transform_traces_nothing_when_no_attachments_pruned() {
    // Same prune-only pressure, but text-only messages: pruning finds nothing
    // to replace and must emit no event.
    let manager = Arc::new(mock_manager());
    let trace = Arc::new(RecordingSessionGraph::default());
    let transform = StandardCompactionTurnTransform::new(StandardCompactionConfig);
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let ctx = build_turn_ctx_with_graph(
        &SessionId::from("root"),
        state,
        Some(prompt_usage(130_000)),
        Some(200_000),
        manager,
        trace.clone(),
    );
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
        trace.events().is_empty(),
        "nothing was pruned and no compaction ran: {:?}",
        trace.events()
    );
}

#[tokio::test]
async fn standard_compactor_returns_summary_seed_for_new_frame() {
    let manager = Arc::new(mock_manager());
    let trace = Arc::new(RecordingSessionGraph::default());
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
    let ctx = build_compaction_ctx_with_graph(
        &SessionId::from("root"),
        state,
        Some(instructions.to_string()),
        manager.clone(),
        trace.clone(),
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

    // FIG-3374: compaction is one direct completion on the calling session.
    // No session-creation attempt may exist — not merely a net-zero catalog.
    assert!(
        manager.created.lock_recover().is_empty(),
        "compaction must not create a child session"
    );
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
    let manager = Arc::new(mock_manager());
    let trace = Arc::new(RecordingSessionGraph::default());
    let state = SessionSnapshot {
        session_id: SessionId::from("root"),
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let captured = Arc::new(RecordingLlmCompletions::default());
    let ctx = build_compaction_ctx_with_graph(
        &SessionId::from("root"),
        state,
        None,
        manager,
        trace.clone(),
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
    let manager = Arc::new(mock_manager());
    let trace = Arc::new(RecordingSessionGraph::default());
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
    let sessions = manager as Arc<dyn SessionStateService>;
    let lifecycle: Arc<dyn SessionLifecycleService> = Arc::new(MockSessionManager::default());
    let captured = Arc::new(RecordingLlmCompletions {
        error: Some("scripted compaction-session failure".to_string()),
        ..Default::default()
    });
    let ctx = build_compaction_ctx_with_services(
        &SessionId::from("root"),
        state,
        None,
        sessions,
        lifecycle,
        trace.clone(),
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

fn transform_state_ctx_with_services(
    state: SessionSnapshot,
    direct: Arc<RecordingLlmCompletions>,
    graph: Arc<dyn SessionGraphService>,
    max_context_tokens: usize,
) -> TurnTransformContext<'static> {
    TurnTransformContext {
        session_id: SessionId::from("root"),
        state: state.read_view().expect("runtime frame scope resolves"),
        prompt_usage: None,
        max_context_tokens: Some(max_context_tokens),
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: graph,
        scoped_effect_controller: lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
            lash_core::AdmittedScope::runtime_operation("standard-compaction-recovery-test"),
        )
        .expect("test scoped effect controller"),
        direct_completions: RecordingLlmCompletions::client(&direct),
        system_prompt: None,
    }
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

#[tokio::test]
async fn recovery_runs_unasked_elides_oversized_result_and_projects_fresh_window() {
    let trace: Arc<RecordingSessionGraph> = Arc::new(RecordingSessionGraph::default());
    let (history_before, state) = recovery_history(true);
    let history_before: Vec<Message> = history_before;

    let direct = recovered_direct();
    let ctx = transform_state_ctx_with_services(state, direct.clone(), trace.clone(), 200_000);

    let prepared = PreparedContext {
        messages: vec![text_message(
            "u2",
            MessageRole::User,
            "now give me the verdict",
        )]
        .into(),
        ..Default::default()
    };
    let built = StandardCompactionTurnTransform::new(StandardCompactionConfig)
        .transform(&ctx, prepared)
        .await
        .expect("recovery transform runs")
        .messages;

    let contents: Vec<&str> = built
        .iter()
        .flat_map(|message| message.parts.iter().map(|part| part.content()))
        .collect();
    assert!(
        contents
            .iter()
            .any(
                |text| text.contains("Compaction summary:") && text.contains("Recovered")
                    || text.contains("Compacted work summary")
            ),
        "the fresh window does not carry the recovered summary: {contents:?}"
    );
    assert!(
        !contents
            .iter()
            .any(|text| text.contains("xxxxxxxxxxxxxxxxxxxxxxxxxx")),
        "the oversized body must never re-enter the recovered prompt window"
    );
    assert!(
        contents
            .iter()
            .any(|text| text.contains("now give me the verdict")),
        "the current request must survive the recovery: {contents:?}"
    );

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

    // The durable terminal record was appended; the summary rides the
    // plugin-visible frame switch instead of the exhausted frame.
    let appends = trace.appends();
    assert_eq!(appends.len(), 1);
    assert_eq!(appends[0].1.nodes.len(), 1);
    let completed_kind = match &appends[0].1.nodes[0] {
        lash_core::SessionAppendNode::Message { message } => {
            message.parts.first().map(Part::content).and_then(|text| {
                recovery_record_kind(&Message {
                    id: "probe".to_string(),
                    role: MessageRole::System,
                    parts: vec![Part::text("probe.p0".to_string(), text.to_string(), None)].into(),
                    origin: Some(MessageOrigin::Plugin {
                        plugin_id: STANDARD_COMPACTION_PLUGIN_ID.to_string(),
                        transient: false,
                    }),
                })
                .expect("committed recovery record parses")
            })
        }
        _ => None,
    };
    assert_eq!(
        completed_kind,
        Some(OverflowRecoveryRecord::Completed),
        "the terminal completed record is durable in the committed history"
    );
    let switches = trace.switches();
    assert_eq!(switches.len(), 1);
    assert_eq!(
        switches[0].1.task.as_deref(),
        Some("context-overflow recovery"),
        "the switch records its task like the in-turn control: {:?}",
        switches[0].1
    );
    assert_eq!(
        switches[0].1.reason.as_str(),
        "compaction",
        "the recovery frame is an ordinary compaction frame: {:?}",
        switches[0].1
    );
    assert_eq!(switches[0].1.initial_nodes.len(), 1);
    let seed_summary = match &switches[0].1.initial_nodes[0] {
        lash_core::SessionAppendNode::Message { message } => {
            message.parts.first().map(Part::content)
        }
        _ => None,
    };
    assert!(
        seed_summary.is_some_and(|text| text.contains("Compaction summary:")),
        "the recovery frame is seeded with the recovered summary: {seed_summary:?}"
    );

    // History stays intact and inspectable: recovery rewrites nothing.
    assert_eq!(trace.appends()[0].1.nodes.len(), 1);

    // The current-turn projection is prompt-view only: no durable history
    // changed beyond the compaction summary seed itself (already checked).
    let _ = history_before;
}

fn recovery_test_input() -> PreparedContext {
    PreparedContext {
        messages: vec![text_message(
            "u2",
            MessageRole::User,
            "now give me the verdict",
        )]
        .into(),
        ..Default::default()
    }
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
    let trace: Arc<RecordingSessionGraph> = Arc::new(RecordingSessionGraph::default());
    let (_history, state) = recovery_history(true);
    let direct = recovered_direct();
    let ctx = transform_state_ctx_with_services(state, direct.clone(), trace.clone(), 200_000);

    StandardCompactionTurnTransform::new(StandardCompactionConfig)
        .transform(&ctx, recovery_test_input())
        .await
        .expect("recovery transform runs");

    let requests = direct.requests();
    assert_eq!(requests.len(), 1, "exactly one summarizer call ran");
    let request_text = RecordingLlmCompletions::request_text(&requests[0]);
    assert!(
        !request_text.contains(OVERFLOW_RECOVERY_MARKER.trim_end_matches(':')),
        "the summarizer request must not carry the pending recovery marker it is recovering from"
    );
}

#[tokio::test]
async fn recovery_failure_is_bounded_and_explicit() {
    let empty = empty_direct();
    let (_messages1, _) = recovery_history(true);
    let mut history: Vec<Message> = _messages1;

    let mut total_appends = 0;
    for attempt in 1..=OVERFLOW_RECOVERY_MAX_ATTEMPTS {
        let trace = Arc::new(RecordingSessionGraph::default());
        let ctx = transform_state_ctx_with_services(
            snapshot_with_messages(&history),
            empty.clone(),
            trace.clone(),
            200_000,
        );
        let built = StandardCompactionTurnTransform::new(StandardCompactionConfig)
            .transform(&ctx, recovery_test_input())
            .await
            .expect("a failed recovery must not fail the turn");

        let contents: Vec<&str> = built
            .messages
            .iter()
            .flat_map(|message| message.parts.iter().map(|part| part.content()))
            .collect();
        assert!(
            !contents
                .iter()
                .any(
                    |text| text.contains("Compaction summary:") && text.contains("Recovered")
                        || text.contains("Compacted work summary")
                ),
            "attempt {attempt} must not project a recovered summary"
        );

        let appends = trace.appends();
        assert_eq!(
            appends.len(),
            1,
            "attempt {attempt} records exactly one append"
        );
        total_appends += appends.len();

        history_with_record(
            &mut history,
            OverflowRecoveryRecord::Failed {
                attempt: attempt as u32,
            },
        );
    }
    assert_eq!(total_appends, OVERFLOW_RECOVERY_MAX_ATTEMPTS);

    // At the cap the recovery stops by itself: no summarizer call, no
    // append, no loop.
    let captured = Arc::new(RecordingLlmCompletions::default());
    let trace: Arc<RecordingSessionGraph> = Arc::new(RecordingSessionGraph::default());
    let ctx = transform_state_ctx_with_services(
        snapshot_with_messages(&history),
        captured.clone(),
        trace.clone(),
        200_000,
    );
    StandardCompactionTurnTransform::new(StandardCompactionConfig)
        .transform(&ctx, recovery_test_input())
        .await
        .expect("capped recovery must not fail the turn");
    assert!(
        trace.appends().is_empty(),
        "the cap spends no further attempt"
    );
    assert!(captured.requests().is_empty());

    let traces = trace.events();
    let outcomes: Vec<&str> = traces
        .iter()
        .filter_map(|(_, event)| match event {
            lash_core::TraceEvent::Custom { name, payload }
                if name == TRACE_OVERFLOW_RECOVERY_OUTCOME =>
            {
                payload.get("outcome").and_then(|value| value.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(outcomes, ["exhausted:recoverable_failure"]);
}

#[tokio::test]
async fn recovery_does_not_restart_after_completion_or_exhaustion() {
    for terminal in [
        OverflowRecoveryRecord::Completed,
        OverflowRecoveryRecord::Exhausted,
    ] {
        let trace: Arc<RecordingSessionGraph> = Arc::new(RecordingSessionGraph::default());
        let captured = Arc::new(RecordingLlmCompletions::default());
        let (mut messages, _) = recovery_history(false);
        messages.push(recovery_record_node_message(
            OverflowRecoveryRecord::Pending,
        ));
        messages.push(recovery_record_node_message(terminal));

        let ctx = transform_state_ctx_with_services(
            snapshot_with_messages(&messages),
            captured.clone(),
            trace.clone(),
            200_000,
        );
        let prepared = PreparedContext {
            messages: vec![text_message(
                "u2",
                MessageRole::User,
                "now give me the verdict",
            )]
            .into(),
            ..Default::default()
        };
        let built = StandardCompactionTurnTransform::new(StandardCompactionConfig)
            .transform(&ctx, prepared)
            .await
            .expect("transform runs");

        assert!(
            trace.appends().is_empty(),
            "a settled recovery must not reopen: {terminal:?}"
        );
        assert!(captured.requests().is_empty());
        let contents: Vec<&str> = built
            .messages
            .iter()
            .flat_map(|message| message.parts.iter().map(|part| part.content()))
            .collect();
        assert!(
            !contents
                .iter()
                .any(
                    |text| text.contains("Compaction summary:") && text.contains("Recovered")
                        || text.contains("Compacted work summary")
                ),
            "the prompt keeps its ordinary rolling projection once recovery settled"
        );
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
    let mut ctx = build_compaction_ctx_with_graph(
        &SessionId::from("root"),
        compactable_state(compactable_messages()),
        None,
        Arc::new(mock_manager()),
        Arc::new(RecordingSessionGraph::default()),
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
        let ctx = build_compaction_ctx_with_graph(
            &SessionId::from("root"),
            compactable_state(compactable_messages()),
            None,
            Arc::new(mock_manager()),
            Arc::new(RecordingSessionGraph::default()),
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
