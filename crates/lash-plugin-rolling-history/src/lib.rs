//! Default rolling-history plugin.
//!
//! Owns rolling prompt-view shaping and the explicit `/compact`
//! summarization strategy.
//!
//! Registered as a default plugin by the first-party default tool bundles,
//! so standard lash sessions pick it up automatically.

use lash_sansio::{SessionId, TurnId};

mod recovery;

pub(crate) use recovery::{
    OverflowRecoveryState, emit_recovery_trace, history_recovery_records,
    overflow_recovery_after_turn, run_overflow_recovery,
};
use std::{collections::BTreeMap, sync::Arc};

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
pub(crate) const COMPACTION_SUMMARY_TITLE: &str = "Compaction summary:";
const COMPACTION_PROMPT: &str = "Provide a detailed summary of the conversation above so a later session can continue the work without the full history.\n\nUse this template:\n---\n## Goal\n[What is the user trying to accomplish?]\n\n## Instructions\n- [Relevant instructions or constraints]\n\n## Discoveries\n[Important findings, failures, or decisions]\n\n## Accomplished\n[What is done, what is in progress, what remains]\n\n## Relevant files / directories\n[List important files or directories]\n---";
const PRUNED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment omitted from older context]";
const COMPACTED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment omitted during compaction]";

/// Maximum summarization attempts one open context-overflow recovery may
/// spend before the third context policy records an explicit recoverable
/// failure. Recovery never loops: every attempt is settled by a durable
/// plugin record.
pub const OVERFLOW_RECOVERY_MAX_ATTEMPTS: usize = 3;
/// Approximate token size above which a single part is elided before an
/// out-of-band summarization request so the summarizer prompt itself fits the
/// model's context window.
pub const OVERFLOW_RECOVERY_ELIDE_PART_THRESHOLD_TOKENS: usize = 16_000;
/// Characters retained from an elided part's head so the summary can still
/// name what it dropped.
pub const OVERFLOW_RECOVERY_ELIDED_RETAINED_CHARS: usize = 400;

const OVERFLOW_RECOVERY_MARKER: &str =
    "Rolling-history context-overflow recovery marker (pending):";
const OVERFLOW_RECOVERY_COMPLETED: &str = "Rolling-history context-overflow recovery completed:";
const OVERFLOW_RECOVERY_FAILED: &str = "Rolling-history context-overflow recovery failure:";
const OVERFLOW_RECOVERY_EXHAUSTED: &str =
    "Rolling-history context-overflow recovery exhausted (recoverable failure):";
const OVERFLOW_RECOVERY_INSTRUCTIONS: &str = "Recover a task whose turn stopped because the provider refused the request as too long. The oversized tool result has been elided from the history below.\n\nSummarize precisely what the user asked for, what was already accomplished, and what remains, so a fresh continuation can finish the task without re-running any tool.";
const OVERFLOW_ELIDED_PART_PLACEHOLDER: &str =
    "[oversized part elided before context-overflow summarization]";
const TRACE_OVERFLOW_RECOVERY_TRIGGER: &str = "rolling_history.overflow_recovery.triggered";
const TRACE_OVERFLOW_RECOVERY_OUTCOME: &str = "rolling_history.overflow_recovery.outcome";

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

pub(crate) fn leading_system_prefix_len(msgs: &[Message]) -> usize {
    msgs.iter()
        .take_while(|msg| msg.role == MessageRole::System)
        .count()
}

pub(crate) fn approx_token_count(text: &str) -> usize {
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

pub(crate) fn is_compaction_summary_message(message: &Message) -> bool {
    matches!(
        message.origin,
        Some(MessageOrigin::Plugin { ref plugin_id, .. }) if plugin_id == ROLLING_HISTORY_PLUGIN_ID
    )
}

pub(crate) fn latest_user_index(messages: &[Message]) -> Option<usize> {
    messages
        .iter()
        .rposition(|message| matches!(message.role, MessageRole::User))
}

/// Walk backwards from the end keeping ~`COMPACTION_KEEP_RECENT_TOKENS` worth of messages.
/// Returns the index of the first message in the "keep" region — everything before it gets
/// summarized.  The cut always lands on a user-message boundary so we never split a turn.
pub(crate) fn find_compaction_cut_point(messages: &[Message], prefix_len: usize) -> usize {
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

/// The one context-pressure fact both rolling-history decisions consume: a known prompt usage
/// measured against a context window that actually bounds it.  A window of zero bounds nothing,
/// so it carries no pressure at all — the same filter the sans-io section builder applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextPressure {
    context_budget_tokens: usize,
    max_context_tokens: usize,
}

impl ContextPressure {
    fn derive(
        prompt_usage: Option<&PromptUsage>,
        max_context_tokens: Option<usize>,
    ) -> Option<Self> {
        Some(Self {
            context_budget_tokens: prompt_usage?.context_budget_tokens,
            max_context_tokens: max_context_tokens.filter(|value| *value > 0)?,
        })
    }

    fn pruning_needed(&self) -> bool {
        (self.context_budget_tokens as f64 / self.max_context_tokens as f64)
            >= PRUNE_CONTEXT_THRESHOLD
    }

    fn compaction_needed(&self) -> bool {
        self.context_budget_tokens >= compaction_threshold(self.max_context_tokens)
    }
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

pub(crate) fn compaction_threshold(max_context_tokens: usize) -> usize {
    max_context_tokens
        .saturating_sub(ROLLING_HISTORY_COMPACTION_BUFFER_TOKENS.min(max_context_tokens))
}

fn append_identity_field(identity: &mut Vec<u8>, value: &str) {
    identity.extend_from_slice(&(value.len() as u64).to_be_bytes());
    identity.extend_from_slice(value.as_bytes());
}

fn latest_physical_turn_id(state: &SessionSnapshot) -> Result<Option<TurnId>, ContextError> {
    let read_view = state
        .read_view()
        .map_err(|error| ContextError::Session(error.to_string()))?;
    Ok(read_view
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message.origin.as_ref() {
            Some(MessageOrigin::TurnInput { turn_id, .. })
            | Some(MessageOrigin::TurnOutput { turn_id, .. }) => Some(turn_id.clone()),
            _ => None,
        }))
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum CompactionGraphAddress<'a> {
    NodeIndex(u64),
    External(&'a str),
}

#[derive(serde::Serialize)]
struct CompactionGraphNodeIdentity<'a> {
    parent: Option<CompactionGraphAddress<'a>>,
    payload: &'a lash_core::SessionNodePayload,
}

#[derive(serde::Serialize)]
struct CompactionSnapshotIdentity<'a> {
    session_id: &'a SessionId,
    policy: &'a lash_core::SessionPolicy,
    graph_nodes: Vec<CompactionGraphNodeIdentity<'a>>,
    graph_leaf: Option<CompactionGraphAddress<'a>>,
    current_frame: Option<CompactionGraphAddress<'a>>,
    turn_index: usize,
    token_usage: &'a lash_core::TokenUsage,
    last_prompt_usage: &'a Option<PromptUsage>,
    protocol_turn_options: &'a lash_core::ProtocolTurnOptions,
    tool_state_ref: &'a Option<lash_core::store::BlobRef>,
    tool_state_generation: Option<u64>,
    plugin_state_ref: &'a Option<lash_core::store::BlobRef>,
    plugin_state_generations: &'a BTreeMap<String, u64>,
    execution_state_ref: &'a Option<lash_core::store::BlobRef>,
    token_ledger: &'a [lash_core::TokenLedgerEntry],
    checkpoint_ref: &'a Option<lash_core::store::BlobRef>,
}

#[derive(serde::Serialize)]
struct CompactionRequestIdentity<'a> {
    // The graph projection keeps ordered payloads and topology while replacing
    // runtime-minted node ids with indices and excluding observational node
    // timestamps. Reconstructing one request therefore cannot acquire ambient
    // time or randomness, while every child-state payload remains binding.
    snapshot: CompactionSnapshotIdentity<'a>,
    prompt_text: &'a str,
}

fn canonicalize_json_objects(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                canonicalize_json_objects(value);
            }
        }
        serde_json::Value::Object(object) => {
            // Reinsert every object in lexical key order so the wire bytes stay
            // canonical with either serde_json's default map or preserve_order.
            let mut entries = std::mem::take(object).into_iter().collect::<Vec<_>>();
            for (_, value) in &mut entries {
                canonicalize_json_objects(value);
            }
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            object.extend(entries);
        }
        _ => {}
    }
}

fn compaction_graph_address<'a>(
    node_indices: &BTreeMap<&'a str, u64>,
    node_id: &'a str,
) -> CompactionGraphAddress<'a> {
    node_indices
        .get(node_id)
        .copied()
        .map_or(CompactionGraphAddress::External(node_id), |index| {
            CompactionGraphAddress::NodeIndex(index)
        })
}

fn compaction_request_identity(
    snapshot: &SessionSnapshot,
    prompt_text: &str,
) -> Result<String, ContextError> {
    let SessionSnapshot {
        session_id,
        policy,
        agent_frames: _, // derived from the graph
        current_frame_node_id,
        session_graph,
        turn_index,
        token_usage,
        last_prompt_usage,
        protocol_turn_options,
        tool_state_ref,
        tool_state_generation,
        plugin_state_ref,
        plugin_state_generations,
        execution_state_ref,
        token_ledger,
        checkpoint_ref,
    } = snapshot;
    let node_indices = session_graph
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.node_id.as_str(), index as u64))
        .collect::<BTreeMap<_, _>>();
    let graph_nodes = session_graph
        .nodes
        .iter()
        .map(|node| CompactionGraphNodeIdentity {
            parent: node
                .parent_node_id
                .as_deref()
                .map(|node_id| compaction_graph_address(&node_indices, node_id)),
            payload: &node.payload,
        })
        .collect();
    let identity = CompactionRequestIdentity {
        snapshot: CompactionSnapshotIdentity {
            session_id,
            policy,
            graph_nodes,
            graph_leaf: session_graph
                .leaf_node_id
                .as_deref()
                .map(|node_id| compaction_graph_address(&node_indices, node_id)),
            current_frame: current_frame_node_id
                .as_deref()
                .map(|node_id| compaction_graph_address(&node_indices, node_id)),
            turn_index: *turn_index,
            token_usage,
            last_prompt_usage,
            protocol_turn_options,
            tool_state_ref,
            tool_state_generation: *tool_state_generation,
            plugin_state_ref,
            plugin_state_generations,
            execution_state_ref,
            token_ledger,
            checkpoint_ref,
        },
        prompt_text,
    };
    let mut identity = serde_json::to_value(identity).map_err(|error| {
        ContextError::Session(format!(
            "failed to encode rolling-history compaction request identity: {error}"
        ))
    })?;
    canonicalize_json_objects(&mut identity);
    serde_json::to_string(&identity).map_err(|error| {
        ContextError::Session(format!(
            "failed to encode rolling-history compaction request identity: {error}"
        ))
    })
}

pub(crate) fn compaction_child_ids(
    parent_session_id: &SessionId,
    state: &SessionSnapshot,
    request_snapshot: &SessionSnapshot,
    prompt_text: &str,
    execution_scope: &lash_core::ExecutionScope,
) -> Result<(SessionId, TurnId), ContextError> {
    let physical_parent_turn_id = latest_physical_turn_id(state)?
        .or_else(|| execution_scope.turn_id().cloned())
        .unwrap_or_else(|| TurnId::from(execution_scope.id()));
    let journal_scope = execution_scope
        .journal_identity()
        .map_err(|error| ContextError::Session(error.to_string()))?;
    let mut identity = Vec::new();
    append_identity_field(&mut identity, parent_session_id);
    append_identity_field(&mut identity, &physical_parent_turn_id);
    append_identity_field(&mut identity, journal_scope.key());
    let request_identity = compaction_request_identity(request_snapshot, prompt_text)?;
    append_identity_field(&mut identity, &request_identity);
    let discriminator = lash_sansio::core_support::blake3_domain_hash_hex(
        "lash-rolling-history-compaction/v2",
        identity,
    );
    Ok((
        SessionId::from(format!("{parent_session_id}-compaction:{discriminator}")),
        TurnId::from(format!(
            "{physical_parent_turn_id}:rolling-history-compaction:{discriminator}"
        )),
    ))
}

pub(crate) fn prepare_compaction_request(
    state: &SessionSnapshot,
    mut prefix_messages: Vec<Message>,
    instructions: Option<&str>,
) -> Result<(SessionSnapshot, String), ContextError> {
    let mut snapshot = lash_core::runtime::RuntimeSessionState::from_snapshot(state.clone());
    snapshot.policy.turn_budget = lash_core::TurnBudget::bounded(1);
    strip_all_attachments(&mut prefix_messages, COMPACTED_ATTACHMENT_PLACEHOLDER);
    snapshot.set_execution_state_snapshot(None);
    snapshot.last_prompt_usage = None;
    let previous_summary = extract_previous_summary(&prefix_messages);
    snapshot
        .replace_active_read_state(&prefix_messages)
        .map_err(|error| ContextError::Session(error.to_string()))?;
    let base_prompt = match previous_summary {
        Some(previous_summary) => compaction_update_prompt(&previous_summary),
        None => COMPACTION_PROMPT.to_string(),
    };
    Ok((
        snapshot.to_snapshot(),
        with_instructions(&base_prompt, instructions),
    ))
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
    session_id: &SessionId,
    state: &SessionSnapshot,
    prefix_messages: Vec<Message>,
    instructions: Option<&str>,
    session_lifecycle: Arc<dyn lash_core::plugin::runtime_host::SessionLifecycleService>,
    scoped_effect_controller: lash_core::ScopedEffectController<'_>,
) -> Result<Option<String>, ContextError> {
    if prefix_messages.is_empty() {
        return Ok(None);
    }

    let (snapshot, prompt_text) = prepare_compaction_request(state, prefix_messages, instructions)?;

    let (compaction_session_id, turn_id) = compaction_child_ids(
        session_id,
        state,
        &snapshot,
        &prompt_text,
        scoped_effect_controller.execution_scope(),
    )?;
    let mut policy = snapshot.policy.clone();
    policy.turn_budget = lash_core::TurnBudget::bounded(1);
    let request = SessionCreateRequest::child(
        session_id,
        SessionStartPoint::Snapshot {
            snapshot: Box::new(snapshot),
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

    let request = lash_core::facade_support::SessionTurnRequest::new_runtime_internal_compaction(
        &handle.session_id,
        &turn_id,
        TurnInput {
            items: vec![InputItem::Text { text: prompt_text }],
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: lash_core::TurnContext::default(),
        },
        scoped_effect_controller,
    )
    .map_err(|err| ContextError::Session(err.to_string()))?;
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
    session_id: &SessionId,
    state: &SessionSnapshot,
    messages: &[Message],
    instructions: Option<&str>,
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
        reg.turn()
            .after(Arc::new(|ctx: lash_core::plugin::TurnResultHookContext| {
                Box::pin(async move { overflow_recovery_after_turn(&ctx).await })
                    as std::pin::Pin<
                        Box<
                            dyn Future<
                                    Output = Result<
                                        Vec<lash_core::plugin::AfterTurnPluginDirective>,
                                        PluginError,
                                    >,
                                > + Send,
                        >,
                    >
            }));
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
        // Third context policy: recover an unrecovered context overflow
        // before any rolling-pressure decision. The durable marker plus the
        // terminal records say whether recovery is pending; a completed or
        // exhausted record below the marker closes it.
        let recovery_state =
            OverflowRecoveryState::derive(history_recovery_records(ctx.state.messages()));
        if recovery_state.pending {
            let current_request = {
                let messages_now = input.messages.make_mut();
                latest_user_index(messages_now)
                    .map_or(Vec::new(), |index| messages_now[index..].to_vec())
            };
            let trace_context =
                lash_core::TraceContext::default().for_session(ctx.session_id.clone());
            if recovery_state.exhausted() {
                emit_recovery_trace(
                    &*ctx.session_graph,
                    trace_context,
                    TRACE_OVERFLOW_RECOVERY_OUTCOME,
                    None,
                    Some("exhausted:recoverable_failure"),
                )
                .await?;
                return Ok(input);
            }
            if let Some(projected) = run_overflow_recovery(
                &ctx.session_id,
                ctx.state.messages(),
                &ctx.state.to_snapshot(),
                ctx.session_lifecycle.clone(),
                &*ctx.session_graph,
                &ctx.scoped_effect_controller,
                ctx.state.to_snapshot().current_frame_node_id.as_deref(),
                trace_context,
                recovery_state,
                ctx.max_context_tokens.unwrap_or(0),
                &current_request,
            )
            .await?
            {
                input.messages.replace(projected);
            }
            return Ok(input);
        }

        let Some(pressure) =
            ContextPressure::derive(ctx.prompt_usage.as_ref(), ctx.max_context_tokens)
        else {
            return Ok(input);
        };

        let needs_pruning = pressure.pruning_needed();
        let needs_compaction = pressure.compaction_needed();
        if !needs_pruning && !needs_compaction {
            return Ok(input);
        }

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
                        context_budget_tokens: pressure.context_budget_tokens,
                        max_context_tokens: pressure.max_context_tokens,
                        threshold_tokens: compaction_threshold(pressure.max_context_tokens),
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
                        context_budget_tokens: pressure.context_budget_tokens,
                        max_context_tokens: pressure.max_context_tokens,
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
                    context_budget_tokens: pressure.context_budget_tokens,
                    max_context_tokens: pressure.max_context_tokens,
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
        let session_lifecycle = Arc::clone(&ctx.session_lifecycle);
        let scoped_effect_controller = ctx.scoped_effect_controller.clone();

        let compaction = compact_messages_core(
            &session_id,
            &ctx.state.to_snapshot(),
            ctx.state.messages(),
            ctx.instructions.as_deref(),
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
#[cfg(test)]
mod tests;
