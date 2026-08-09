//! Default rolling-history plugin.
//!
//! Owns rolling prompt-view shaping and the explicit `/compact`
//! summarization strategy.
//!
//! Registered as a default plugin by
//! the first-party default tool bundles from `lash-standard-plugins`,
//! so standard lash sessions pick it up automatically.

use std::sync::Arc;

use async_trait::async_trait;

use lash_core::facade_support::PreparedContext;
use lash_core::plugin::{
    CompactionContext, ContextCompaction, ContextCompactor, ContextError, PluginError,
    PluginFactory, PluginOptions, PluginRegistrar, PluginSessionContext, SessionContextOverlay,
    SessionCreateRequest, SessionPlugin, SessionStartPoint, TurnContextTransform,
    TurnTransformContext,
};
use lash_core::{
    InputItem, Message, MessageOrigin, MessageRole, Part, PartKind, PromptUsage, SessionSnapshot,
    TurnInput,
};

const PRUNE_RECENT_USER_TURNS: usize = 2;
pub const ROLLING_HISTORY_COMPACTION_BUFFER_TOKENS: usize = 20_000;
const COMPACTION_KEEP_RECENT_TOKENS: usize = 20_000;
const PRUNE_CONTEXT_THRESHOLD: f64 = 0.6;
/// Marker `plugin_id` stamped on compaction summary messages so the
/// history pipeline can recognize them on subsequent turns.
pub(crate) const ROLLING_HISTORY_PLUGIN_ID: &str = "rolling_history";
const COMPACTION_SUMMARY_TITLE: &str = "Compaction summary:";
const COMPACTION_PROMPT: &str = "Provide a detailed summary of the conversation above so a later session can continue the work without the full history.\n\nUse this template:\n---\n## Goal\n[What is the user trying to accomplish?]\n\n## Instructions\n- [Relevant instructions or constraints]\n\n## Discoveries\n[Important findings, failures, or decisions]\n\n## Accomplished\n[What is done, what is in progress, what remains]\n\n## Relevant files / directories\n[List important files or directories]\n---";
const PRUNED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment omitted from older context]";
const COMPACTED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment omitted during compaction]";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RollingHistoryConfig;

fn compaction_update_prompt(previous_summary: &str) -> String {
    format!(
        "A previous compaction summary exists (shown below). Update it with information from the conversation above.\n\n\
         Rules:\n\
         - PRESERVE all existing information from the previous summary\n\
         - ADD new progress, decisions, and context from the new messages\n\
         - Move items from in-progress to done where applicable\n\
         - PRESERVE exact file paths, function names, and error messages\n\n\
         Previous summary:\n{previous_summary}\n\n\
         Use this template:\n---\n\
         ## Goal\n[What is the user trying to accomplish?]\n\n\
         ## Instructions\n- [Relevant instructions or constraints]\n\n\
         ## Discoveries\n[Important findings, failures, or decisions]\n\n\
         ## Accomplished\n[What is done, what is in progress, what remains]\n\n\
         ## Relevant files / directories\n[List important files or directories]\n---"
    )
}

fn with_instructions(base: &str, instructions: Option<&str>) -> String {
    match instructions {
        Some(text) if !text.trim().is_empty() => {
            format!("{base}\n\nAdditional focus:\n{}\n", text.trim())
        }
        _ => base.to_string(),
    }
}

fn leading_system_prefix_len(msgs: &[Message]) -> usize {
    msgs.iter()
        .take_while(|msg| msg.role == MessageRole::System)
        .count()
}

fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn strip_attachment(part: &mut Part, placeholder: &str) -> bool {
    if !matches!(part.kind, PartKind::Attachment) || part.attachment.is_none() {
        return false;
    }
    part.attachment = None;
    part.content = placeholder.to_string();
    true
}

fn prune_old_attachments(messages: &mut [Message]) -> bool {
    let mut changed = false;
    let mut recent_user_turns = 0usize;

    'scan: for msg_idx in (0..messages.len()).rev() {
        if is_compaction_summary_message(&messages[msg_idx]) {
            break 'scan;
        }
        if messages[msg_idx].role == MessageRole::User {
            recent_user_turns += 1;
        }
        if recent_user_turns < PRUNE_RECENT_USER_TURNS {
            continue;
        }
        for part in std::sync::Arc::make_mut(&mut messages[msg_idx].parts).iter_mut() {
            changed |= strip_attachment(part, PRUNED_ATTACHMENT_PLACEHOLDER);
        }
    }

    changed
}

fn strip_all_attachments(messages: &mut [Message], placeholder: &str) -> bool {
    let mut changed = false;
    for message in messages {
        for part in std::sync::Arc::make_mut(&mut message.parts).iter_mut() {
            changed |= strip_attachment(part, placeholder);
        }
    }
    changed
}

fn is_compaction_summary_message(message: &Message) -> bool {
    matches!(
        message.origin,
        Some(MessageOrigin::Plugin { ref plugin_id, .. }) if plugin_id == ROLLING_HISTORY_PLUGIN_ID
    )
}

fn latest_user_index(messages: &[Message]) -> Option<usize> {
    messages
        .iter()
        .rposition(|message| matches!(message.role, MessageRole::User))
}

/// Walk backwards from the end keeping ~`COMPACTION_KEEP_RECENT_TOKENS` worth of messages.
/// Returns the index of the first message in the "keep" region — everything before it gets
/// summarized.  The cut always lands on a user-message boundary so we never split a turn.
fn find_compaction_cut_point(messages: &[Message], prefix_len: usize) -> usize {
    let start = messages[prefix_len..]
        .iter()
        .rposition(is_compaction_summary_message)
        .map(|i| prefix_len + i + 1)
        .unwrap_or(prefix_len);

    let mut accumulated = 0usize;
    for idx in (start..messages.len()).rev() {
        for part in messages[idx].parts.iter() {
            accumulated += approx_token_count(&part.content);
            if part.attachment.is_some() {
                accumulated += 1200; // approximate binary attachment token cost
            }
        }
        if accumulated >= COMPACTION_KEEP_RECENT_TOKENS && messages[idx].role == MessageRole::User {
            return idx;
        }
    }
    latest_user_index(messages).unwrap_or(messages.len())
}

fn pruning_needed(prompt_usage: Option<&PromptUsage>, max_context_tokens: Option<usize>) -> bool {
    let Some(usage) = prompt_usage else {
        return false;
    };
    let Some(max_context) = max_context_tokens else {
        return false;
    };
    if max_context == 0 {
        return false;
    }
    (usage.context_budget_tokens as f64 / max_context as f64) >= PRUNE_CONTEXT_THRESHOLD
}

fn extract_previous_summary(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|m| {
        if !is_compaction_summary_message(m) {
            return None;
        }
        m.parts.first().map(|p| {
            p.content
                .strip_prefix(COMPACTION_SUMMARY_TITLE)
                .unwrap_or(&p.content)
                .trim()
                .to_string()
        })
    })
}

fn compaction_needed(
    prompt_usage: Option<&PromptUsage>,
    max_context_tokens: Option<usize>,
) -> bool {
    let Some(usage) = prompt_usage else {
        return false;
    };
    let Some(max_context) = max_context_tokens else {
        return false;
    };
    usage.context_budget_tokens >= compaction_threshold(max_context)
}

fn compaction_threshold(max_context_tokens: usize) -> usize {
    max_context_tokens
        .saturating_sub(ROLLING_HISTORY_COMPACTION_BUFFER_TOKENS.min(max_context_tokens))
}

fn compaction_turn_id(parent_turn_id: &str) -> String {
    format!("{parent_turn_id}:rolling-history-compaction")
}

fn prompt_tail_window(messages: &[Message], cut_point: usize) -> Vec<Message> {
    let prefix_len = leading_system_prefix_len(messages);
    let latest_summary_index = messages[prefix_len..]
        .iter()
        .rposition(is_compaction_summary_message)
        .map(|index| prefix_len + index);
    let mut out = Vec::new();
    out.extend_from_slice(&messages[..prefix_len]);
    if let Some(summary_index) = latest_summary_index
        && summary_index < cut_point
    {
        out.push(messages[summary_index].clone());
    }
    out.extend_from_slice(&messages[cut_point..]);
    out
}

async fn summarize_compaction_prefix(
    session_id: &str,
    state: &SessionSnapshot,
    prefix_messages: Vec<Message>,
    instructions: Option<&str>,
    sessions: Arc<dyn lash_core::plugin::runtime_host::SessionStateService>,
    session_lifecycle: Arc<dyn lash_core::plugin::runtime_host::SessionLifecycleService>,
    scoped_effect_controller: lash_core::ScopedEffectController<'_>,
) -> Result<Option<String>, ContextError> {
    if prefix_messages.is_empty() {
        return Ok(None);
    }

    let mut snapshot = lash_core::runtime::RuntimeSessionState::from_snapshot(state.clone());
    snapshot.policy.turn_budget = lash_core::TurnBudget::bounded(1);
    let mut messages = prefix_messages;
    strip_all_attachments(&mut messages, COMPACTED_ATTACHMENT_PLACEHOLDER);
    snapshot.execution_state_snapshot = None;
    snapshot.last_prompt_usage = None;
    let previous_summary = extract_previous_summary(&messages);
    snapshot.replace_active_read_state(&messages);

    let compaction_session_id = format!("{session_id}-compaction");
    let mut policy = snapshot.policy.clone();
    policy.turn_budget = lash_core::TurnBudget::bounded(1);
    let request = SessionCreateRequest::child(
        session_id,
        SessionStartPoint::Snapshot {
            snapshot: Box::new(snapshot.to_snapshot()),
        },
        policy,
        PluginOptions::default(),
        "compaction",
    )
    .with_context_overlay(SessionContextOverlay {
        include_base_tools: false,
        tool_providers: Vec::new(),
        prompt_contributions: Vec::new(),
    })
    .with_session_id(compaction_session_id);
    let handle = session_lifecycle
        .create_session(request)
        .await
        .map_err(ContextError::from)?;

    let base_prompt = match previous_summary {
        Some(prev) => compaction_update_prompt(&prev),
        None => COMPACTION_PROMPT.to_string(),
    };
    let prompt_text = with_instructions(&base_prompt, instructions);

    let turn_id = compaction_turn_id(scoped_effect_controller.scope_id());
    let turn_scope = sessions
        .turn_scope(&handle.session_id, &turn_id)
        .await
        .map_err(ContextError::from)?;
    let compaction_effect_controller = lash_core::ScopedEffectController::borrowed(
        scoped_effect_controller.controller(),
        turn_scope,
    )
    .map_err(|err| ContextError::Session(err.to_string()))?;
    let request = lash_core::facade_support::SessionTurnRequest::new(
        &handle.session_id,
        &turn_id,
        TurnInput {
            items: vec![InputItem::Text { text: prompt_text }],
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: lash_core::TurnContext::default(),
        },
        compaction_effect_controller,
    )
    .map_err(|err| ContextError::Session(err.to_string()))?
    .with_runtime_internal_compaction_admission();
    let turn = session_lifecycle.start_turn(request).await;
    let _ = session_lifecycle.close_session(&handle.session_id).await;
    let turn = turn.map_err(ContextError::from)?;
    let summary = turn.assistant_output.safe_text.trim().to_string();
    if summary.is_empty() {
        return Ok(None);
    }
    Ok(Some(summary))
}

fn compaction_summary_seed(summary: &str) -> lash_core::SessionAppendNode {
    lash_core::SessionAppendNode::message(
        lash_core::PluginMessage::text(
            MessageRole::Assistant,
            format!("{COMPACTION_SUMMARY_TITLE}\n{summary}"),
        )
        .with_origin(MessageOrigin::Plugin {
            plugin_id: ROLLING_HISTORY_PLUGIN_ID.to_string(),
            transient: false,
        }),
    )
}

async fn compact_messages_core(
    session_id: &str,
    state: &SessionSnapshot,
    messages: &[Message],
    instructions: Option<&str>,
    sessions: Arc<dyn lash_core::plugin::runtime_host::SessionStateService>,
    session_lifecycle: Arc<dyn lash_core::plugin::runtime_host::SessionLifecycleService>,
    scoped_effect_controller: lash_core::ScopedEffectController<'_>,
) -> Result<Option<ContextCompaction>, ContextError> {
    let prefix_len = leading_system_prefix_len(messages);
    let cut_point = find_compaction_cut_point(messages, prefix_len);
    if cut_point <= prefix_len {
        return Ok(None);
    }
    let prefix_messages = messages[prefix_len..].to_vec();
    let Some(summary) = summarize_compaction_prefix(
        session_id,
        state,
        prefix_messages,
        instructions,
        sessions,
        session_lifecycle,
        scoped_effect_controller,
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(ContextCompaction::new(vec![compaction_summary_seed(
        &summary,
    )])))
}

pub struct RollingHistoryPluginFactory {
    config: RollingHistoryConfig,
}

impl RollingHistoryPluginFactory {
    pub fn new(config: RollingHistoryConfig) -> Self {
        Self { config }
    }
}

impl Default for RollingHistoryPluginFactory {
    fn default() -> Self {
        Self::new(RollingHistoryConfig)
    }
}

impl PluginFactory for RollingHistoryPluginFactory {
    fn id(&self) -> &'static str {
        ROLLING_HISTORY_PLUGIN_ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(RollingHistoryPlugin {
            config: self.config.clone(),
        }))
    }
}

struct RollingHistoryPlugin {
    config: RollingHistoryConfig,
}

impl SessionPlugin for RollingHistoryPlugin {
    fn id(&self) -> &'static str {
        ROLLING_HISTORY_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let config = self.config.clone();
        reg.context()
            .prepare_turn(100, Arc::new(RollingTurnTransform::new(config.clone())));
        reg.context()
            .compact(100, Arc::new(RollingContextCompactor::new(config)));
        Ok(())
    }
}

struct RollingTurnTransform;

impl RollingTurnTransform {
    fn new(_config: RollingHistoryConfig) -> Self {
        Self
    }
}

#[async_trait]
impl TurnContextTransform for RollingTurnTransform {
    fn id(&self) -> &'static str {
        "rolling_history.prepare_turn"
    }

    async fn transform(
        &self,
        ctx: &TurnTransformContext<'_>,
        mut input: PreparedContext,
    ) -> Result<PreparedContext, ContextError> {
        let prompt_usage = ctx.prompt_usage.as_ref();
        let max_context_tokens = ctx.max_context_tokens;

        let needs_pruning = pruning_needed(prompt_usage, max_context_tokens);
        let needs_compaction = compaction_needed(prompt_usage, max_context_tokens);
        if !needs_pruning && !needs_compaction {
            return Ok(input);
        }
        let (Some(usage), Some(max_context_tokens)) = (prompt_usage, max_context_tokens) else {
            unreachable!("rolling-history decisions require prompt usage and a context window")
        };

        let mut trace_context =
            lash_core::TraceContext::default().for_session(ctx.session_id.clone());
        if let Some(turn_id) = ctx.scoped_effect_controller.turn_id() {
            trace_context = trace_context.for_turn(turn_id);
        }
        if needs_compaction {
            ctx.session_graph
                .emit_trace_event(
                    trace_context.clone(),
                    lash_core::TraceEvent::RollingHistoryCompactionNeeded {
                        context_budget_tokens: usage.context_budget_tokens,
                        max_context_tokens,
                        threshold_tokens: compaction_threshold(max_context_tokens),
                    },
                )
                .await?;
        }

        let messages = input.messages.make_mut();

        if needs_pruning {
            prune_old_attachments(messages);
        }

        if !needs_compaction {
            return Ok(input);
        }

        let messages = input.messages.make_mut();
        let prefix_len = leading_system_prefix_len(messages);
        let cut_point = find_compaction_cut_point(messages, prefix_len);
        if cut_point <= prefix_len {
            ctx.session_graph
                .emit_trace_event(
                    trace_context,
                    lash_core::TraceEvent::RollingHistoryPromptPruned {
                        context_budget_tokens: usage.context_budget_tokens,
                        max_context_tokens,
                        dropped_prefix_messages: 0,
                        retained_messages: messages.len(),
                    },
                )
                .await?;
            return Ok(input);
        }

        let message_count = messages.len();
        let projected = prompt_tail_window(messages, cut_point);
        let dropped_prefix_messages = message_count.saturating_sub(projected.len());
        let retained_messages = projected.len();
        input.messages.replace(projected);
        ctx.session_graph
            .emit_trace_event(
                trace_context,
                lash_core::TraceEvent::RollingHistoryPromptPruned {
                    context_budget_tokens: usage.context_budget_tokens,
                    max_context_tokens,
                    dropped_prefix_messages,
                    retained_messages,
                },
            )
            .await?;
        Ok(input)
    }
}

struct RollingContextCompactor;

impl RollingContextCompactor {
    fn new(_config: RollingHistoryConfig) -> Self {
        Self
    }
}

#[async_trait]
impl ContextCompactor for RollingContextCompactor {
    fn id(&self) -> &'static str {
        "rolling_history.compact"
    }

    async fn compact(
        &self,
        ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError> {
        let mut trace_context =
            lash_core::TraceContext::default().for_session(ctx.session_id.clone());
        if let Some(turn_id) = ctx.scoped_effect_controller.turn_id() {
            trace_context = trace_context.for_turn(turn_id);
        }
        ctx.session_graph
            .emit_trace_event(
                trace_context.clone(),
                lash_core::TraceEvent::RollingHistoryCompactionStarted {
                    source_messages: ctx.state.messages().len(),
                    instructions_present: ctx
                        .instructions
                        .as_deref()
                        .is_some_and(|instructions| !instructions.trim().is_empty()),
                },
            )
            .await?;

        let session_id = ctx.session_id.clone();
        let sessions = Arc::clone(&ctx.sessions);
        let session_lifecycle = Arc::clone(&ctx.session_lifecycle);
        let scoped_effect_controller = ctx.scoped_effect_controller.clone();

        let compaction = compact_messages_core(
            &session_id,
            &ctx.state.to_snapshot(),
            ctx.state.messages(),
            ctx.instructions.as_deref(),
            sessions,
            session_lifecycle,
            scoped_effect_controller,
        )
        .await;
        let summary_nodes = compaction
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map_or(0, |compaction| compaction.initial_nodes.len());
        ctx.session_graph
            .emit_trace_event(
                trace_context,
                lash_core::TraceEvent::RollingHistoryCompactionCompleted { summary_nodes },
            )
            .await?;
        compaction
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_sansio::sync::MutexExt;
    use std::sync::Mutex;

    use lash_core::plugin::{SessionGraphService, SessionLifecycleService, SessionStateService};
    use lash_core::{SessionGraph, SessionPolicy};
    use serde_json::json;

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
                        id: lash_core::AttachmentId::new(format!("{id}-att")),
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
            .with_turn(empty_turn("root", "Compacted work summary"))
    }

    #[derive(Default)]
    struct RecordingSessionGraph {
        events: Mutex<Vec<(lash_core::TraceContext, lash_core::TraceEvent)>>,
    }

    impl RecordingSessionGraph {
        fn events(&self) -> Vec<(lash_core::TraceContext, lash_core::TraceEvent)> {
            self.events.lock_recover().clone()
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
    }

    fn build_turn_ctx(
        session_id: &str,
        state: SessionSnapshot,
        prompt_usage: Option<PromptUsage>,
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
        session_id: &str,
        state: SessionSnapshot,
        prompt_usage: Option<PromptUsage>,
        max_context_tokens: Option<usize>,
        manager: Arc<MockSessionManager>,
        session_graph: Arc<dyn SessionGraphService>,
    ) -> TurnTransformContext<'static> {
        TurnTransformContext {
            session_id: session_id.to_string(),
            state: state.read_view(),
            prompt_usage,
            max_context_tokens,
            sessions: manager.clone(),
            session_lifecycle: manager.clone(),
            session_graph,
            scoped_effect_controller: lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::turn(session_id, "rolling-history-test-turn"),
            )
            .expect("test scoped effect controller"),
            direct_completions: lash_core::facade_support::DirectCompletionClient::from_fn(
                |_, _| {
                    Err(lash_core::PluginError::Session(
                        "direct completions are unavailable in rolling history tests".to_string(),
                    ))
                },
            ),
        }
    }

    fn build_compaction_ctx_with_graph(
        session_id: &str,
        state: SessionSnapshot,
        instructions: Option<String>,
        manager: Arc<MockSessionManager>,
        session_graph: Arc<dyn SessionGraphService>,
    ) -> CompactionContext<'static> {
        let sessions = manager.clone();
        build_compaction_ctx_with_services(
            session_id,
            state,
            instructions,
            sessions,
            manager,
            session_graph,
        )
    }

    fn build_compaction_ctx_with_services(
        session_id: &str,
        state: SessionSnapshot,
        instructions: Option<String>,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
    ) -> CompactionContext<'static> {
        CompactionContext {
            session_id: session_id.to_string(),
            instructions,
            state: state.read_view(),
            sessions,
            session_lifecycle,
            session_graph,
            scoped_effect_controller: lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::runtime_operation("rolling-history-compact-test"),
            )
            .expect("test scoped effect controller"),
        }
    }

    struct FailingSessionLifecycle;

    #[async_trait]
    impl SessionLifecycleService for FailingSessionLifecycle {
        async fn create_session(
            &self,
            _request: SessionCreateRequest,
        ) -> Result<lash_core::plugin::SessionHandle, PluginError> {
            Err(PluginError::Session(
                "scripted compaction-session failure".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn rolling_turn_transform_strips_old_image_attachments() {
        let messages = vec![
            image_message("u0", MessageRole::User, &[1, 2, 3]),
            text_message("u1", MessageRole::User, "recent"),
            text_message("u2", MessageRole::User, "latest"),
        ];

        let state = SessionSnapshot::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ));
        let manager = Arc::new(mock_manager());
        let transform = RollingTurnTransform::new(RollingHistoryConfig);
        let ctx = build_turn_ctx(
            "root",
            state,
            Some(PromptUsage {
                prompt_context_tokens: 130_000,
                input_tokens: 130_000,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                context_budget_tokens: 130_000,
            }),
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
        assert!(matches!(image_part.kind, PartKind::Attachment));
        assert!(image_part.attachment.is_none());
        assert_eq!(image_part.content, PRUNED_ATTACHMENT_PLACEHOLDER);
    }

    #[tokio::test]
    async fn rolling_turn_transform_projects_tail_without_summary() {
        let manager = Arc::new(mock_manager());
        let transform = RollingTurnTransform::new(RollingHistoryConfig);
        let state = SessionSnapshot {
            session_id: "root".to_string(),
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
        };
        let ctx = build_turn_ctx(
            "root",
            state,
            Some(PromptUsage {
                prompt_context_tokens: 90_000,
                input_tokens: 90_000,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                context_budget_tokens: 90_000,
            }),
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
                .any(|part| part.content.contains("latest request"))
        }));
        assert!(!built.iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| part.content.contains("old work"))
        }));

        let created = manager.created_snapshot();
        assert!(created.is_empty());
        let turns = manager.turns.lock_recover().clone();
        assert!(turns.is_empty());
    }

    #[tokio::test]
    async fn rolling_turn_transform_traces_threshold_and_prompt_pruning() {
        let manager = Arc::new(mock_manager());
        let trace = Arc::new(RecordingSessionGraph::default());
        let transform = RollingTurnTransform::new(RollingHistoryConfig);
        let state = SessionSnapshot {
            session_id: "root".to_string(),
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
        };
        let ctx = build_turn_ctx_with_graph(
            "root",
            state,
            Some(PromptUsage {
                prompt_context_tokens: 30_000,
                input_tokens: 30_000,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                context_budget_tokens: 30_000,
            }),
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
            Some("rolling-history-test-turn")
        );
        assert_eq!(
            events[0].1,
            lash_core::TraceEvent::RollingHistoryCompactionNeeded {
                context_budget_tokens: 30_000,
                max_context_tokens: 40_000,
                threshold_tokens: 20_000,
            }
        );
        assert_eq!(
            events[1].1,
            lash_core::TraceEvent::RollingHistoryPromptPruned {
                context_budget_tokens: 30_000,
                max_context_tokens: 40_000,
                dropped_prefix_messages: 2,
                retained_messages: 1,
            }
        );
    }

    #[tokio::test]
    async fn rolling_turn_transform_records_needed_when_no_cut_point_exists() {
        let manager = Arc::new(mock_manager());
        let trace = Arc::new(RecordingSessionGraph::default());
        let transform = RollingTurnTransform::new(RollingHistoryConfig);
        let state = SessionSnapshot {
            session_id: "root".to_string(),
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
        };
        let ctx = build_turn_ctx_with_graph(
            "root",
            state,
            Some(PromptUsage {
                prompt_context_tokens: 30_000,
                input_tokens: 30_000,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                context_budget_tokens: 30_000,
            }),
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
            lash_core::TraceEvent::RollingHistoryPromptPruned {
                context_budget_tokens: 30_000,
                max_context_tokens: 40_000,
                dropped_prefix_messages: 0,
                retained_messages: 2,
            }
        );
    }

    #[tokio::test]
    async fn rolling_compactor_returns_summary_seed_for_new_frame() {
        let manager = Arc::new(mock_manager());
        let trace = Arc::new(RecordingSessionGraph::default());
        let messages = vec![
            text_message("u1", MessageRole::User, "old work"),
            text_message("a1", MessageRole::Assistant, "assistant old"),
            text_message("u2", MessageRole::User, "latest request"),
        ];
        let state = SessionSnapshot {
            session_id: "root".to_string(),
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            session_graph: SessionGraph::from_active_read_state(&messages),
            ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
        };
        let ctx = build_compaction_ctx_with_graph(
            "root",
            state,
            Some("focus on latest request".to_string()),
            manager.clone(),
            trace.clone(),
        );
        let compactor = RollingContextCompactor::new(RollingHistoryConfig);

        let compaction = compactor
            .compact(&ctx)
            .await
            .expect("compact")
            .expect("compaction");

        assert_eq!(compaction.initial_nodes.len(), 1);
        let lash_core::SessionAppendNode::Message { message, .. } = &compaction.initial_nodes[0]
        else {
            panic!("expected summary message seed");
        };
        assert_eq!(message.role, MessageRole::Assistant);
        assert!(
            message
                .first_text()
                .expect("summary text")
                .contains("Compacted work summary")
        );
        assert!(matches!(
            message.origin.as_ref(),
            Some(MessageOrigin::Plugin { plugin_id, .. }) if plugin_id == ROLLING_HISTORY_PLUGIN_ID
        ));

        let created = manager.created_snapshot();
        assert_eq!(created.len(), 1);
        let turns = manager.turns.lock_recover().clone();
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].1,
            "rolling-history-compact-test:rolling-history-compaction"
        );
        assert_eq!(
            turns[0].2.as_deref(),
            Some("rolling-history-compact-test:rolling-history-compaction")
        );
        assert_eq!(
            turns[0].3,
            lash_core::ExecutionScope::turn(
                "root-compaction",
                "rolling-history-compact-test:rolling-history-compaction"
            )
        );

        let events = trace.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0.session_id.as_deref(), Some("root"));
        assert_eq!(events[0].0.turn_id, None);
        assert_eq!(
            events[0].1,
            lash_core::TraceEvent::RollingHistoryCompactionStarted {
                source_messages: 3,
                instructions_present: true,
            }
        );
        assert_eq!(
            events[1].1,
            lash_core::TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 1 }
        );
    }

    #[tokio::test]
    async fn rolling_compactor_records_zero_node_completion_for_none() {
        let manager = Arc::new(mock_manager());
        let trace = Arc::new(RecordingSessionGraph::default());
        let state = SessionSnapshot {
            session_id: "root".to_string(),
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
        };
        let ctx = build_compaction_ctx_with_graph("root", state, None, manager, trace.clone());

        let compaction = RollingContextCompactor::new(RollingHistoryConfig)
            .compact(&ctx)
            .await
            .expect("empty history is a successful no-op");

        assert!(compaction.is_none());
        assert_eq!(
            trace.events()[1].1,
            lash_core::TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 0 }
        );
    }

    #[tokio::test]
    async fn rolling_compactor_records_zero_node_completion_before_error() {
        let manager = Arc::new(mock_manager());
        let trace = Arc::new(RecordingSessionGraph::default());
        let messages = vec![
            text_message("u1", MessageRole::User, "old work"),
            text_message("a1", MessageRole::Assistant, "assistant old"),
            text_message("u2", MessageRole::User, "latest request"),
        ];
        let state = SessionSnapshot {
            session_id: "root".to_string(),
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            session_graph: SessionGraph::from_active_read_state(&messages),
            ..SessionSnapshot::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
        };
        let sessions = manager as Arc<dyn SessionStateService>;
        let ctx = build_compaction_ctx_with_services(
            "root",
            state,
            None,
            sessions,
            Arc::new(FailingSessionLifecycle),
            trace.clone(),
        );

        let error = RollingContextCompactor::new(RollingHistoryConfig)
            .compact(&ctx)
            .await
            .expect_err("scripted lifecycle failure must propagate");

        assert!(
            error
                .to_string()
                .contains("scripted compaction-session failure")
        );
        assert_eq!(
            trace.events()[1].1,
            lash_core::TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 0 }
        );
    }
}
