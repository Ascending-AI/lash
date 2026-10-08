//! Default standard-compaction plugin.
//!
//! Owns the standard protocol's context policies: old-attachment pruning in
//! the request's view of history, compaction — an explicit administrative compaction or at
//! the context-pressure threshold — and context-overflow recovery. Every
//! compaction starts a fresh frame seeded with its summary (FIG-4029): a
//! frame is the context window.
//!
//! Pruning is an attachment-omission history policy and stays ephemeral (ADR
//! 0133): it names old attachments, core omits them from the request with
//! one placeholder, and the history keeps them. The durable policies return
//! decisions core writes: `compact_context` through the
//! [`ContextCompactor`], and the pressure threshold and overflow recovery
//! through the [`ContextPressureHook`], which core calls once per turn before
//! the history policies (FIG-4110).
//!
//! The standard protocol's plugin only: RLM switches frames through the
//! model-driven `continue_as`.

/// version_surface = "coexist"
/// version_guard(items(LASH_STANDARD_COMPACTION_DOMAIN_VERSION, compaction_request_ids))
const LASH_STANDARD_COMPACTION_DOMAIN_VERSION: &str = "lash-standard-compaction/v1";

use lash_sansio::{SessionId, TurnId};

mod recovery;

pub(crate) use recovery::{
    OverflowRecoveryState, history_recovery_records, overflow_recovery_after_turn,
    overflow_recovery_decision,
};
use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;

use lash_core::facade_support::ModelToolReturnPart;
use lash_core::plugin::{
    AttachmentOmissionContext, AttachmentOmissionPolicy, CompactionContext, ContextCompaction,
    ContextCompactor, ContextError, ContextPressureContext, ContextPressureDecision,
    ContextPressureHook, HistoryPartId, PluginError, PluginFactory, PluginRegistrar,
    PluginSessionContext, SessionPlugin, omit_part_attachments,
};
use lash_core::{LlmUsage, Message, MessageOrigin, MessageRole, Part, PartKind, SessionSnapshot};

/// Marker `plugin_id` stamped on compaction summary messages so the
/// history pipeline can recognize them on subsequent turns.
pub(crate) const STANDARD_COMPACTION_PLUGIN_ID: &str = "standard_compaction";
pub(crate) const COMPACTION_SUMMARY_TITLE: &str = "Compaction summary:";
const COMPACTION_PROMPT: &str = "Provide a detailed summary of the conversation above so a later session can continue the work without the full history.\n\nUse this template:\n---\n## Goal\n[What is the user trying to accomplish?]\n\n## Instructions\n- [Relevant instructions or constraints]\n\n## Discoveries\n[Important findings, failures, or decisions]\n\n## Accomplished\n[What is done, what is in progress, what remains]\n\n## Relevant files / directories\n[List important files or directories]\n---";
/// The section carrying standard compaction's summary instruction.
pub const SUMMARY_INSTRUCTION_SECTION: &str = "summary_instruction";

struct SummaryInstruction(String);

const COMPACTED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment omitted during compaction]";

const OVERFLOW_RECOVERY_INSTRUCTIONS: &str = "Recover a task whose turn stopped because the provider refused the request as too long. The oversized tool result has been elided from the history below.\n\nSummarize precisely what the user asked for, what was already accomplished, and what remains, so a fresh continuation can finish the task without re-running any tool.";
const OVERFLOW_ELIDED_PART_PLACEHOLDER: &str =
    "[oversized part elided before context-overflow summarization]";
const TRACE_OVERFLOW_RECOVERY_TRIGGER: &str = "standard_compaction.overflow_recovery.triggered";
const TRACE_OVERFLOW_RECOVERY_OUTCOME: &str = "standard_compaction.overflow_recovery.outcome";
/// The task a context-pressure compaction frame names.
const PRESSURE_COMPACTION_TASK: &str = "context-pressure compaction";
/// The task a context-overflow recovery frame names.
const OVERFLOW_RECOVERY_TASK: &str = "context-overflow recovery";

/// Host-selected context policies. All cuts affect the request view only;
/// durable history keeps the original parts. Defaults use [`Self::standard`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StandardCompactionConfig {
    pub pressure_compaction: bool,
    pub attachment_pruning: bool,
    pub overflow_recovery: bool,
    pub compaction_buffer_tokens: usize,
    /// Eligibility bound for explicit compaction; the cut lands on a user turn.
    pub keep_recent_tokens: usize,
    /// Recent user turns copied into the fresh frame alongside the summary.
    pub retained_user_turns: usize,
    pub prune_recent_user_turns: usize,
    pub prune_context_percent: u8,
    pub token_bytes: std::num::NonZeroUsize,
    pub attachment_tokens: usize,
    pub recovery_request_overhead_tokens: usize,
    pub overflow_max_attempts: std::num::NonZeroU32,
    pub overflow_elide_part_threshold_tokens: usize,
    pub overflow_elided_retained_chars: usize,
    pub summary_instructions: String,
    /// Template whose `{previous_summary}` slot is replaced with the last summary.
    pub update_instructions: String,
    pub overflow_instructions: String,
}

impl Default for StandardCompactionConfig {
    fn default() -> Self {
        Self::standard()
    }
}

impl StandardCompactionConfig {
    /// The standard preset: pressure, pruning and recovery on; a 20,000-token
    /// buffer and eligibility bound; no copied history; two recent user turns
    /// protected from attachment pruning; pruning at 60%;
    /// one token per four text bytes and 1,200 per attachment; 512 tokens of
    /// request overhead; three overflow attempts; elide parts at 16,000 tokens
    /// keeping 400 characters. Instructions use the Goal/Instructions/
    /// Discoveries/Accomplished/Files template and preserve the prior summary.
    /// These historical choices have no workload measurement establishing them
    /// as universal defaults. Every field can be changed before installation.
    pub fn standard() -> Self {
        Self {
            pressure_compaction: true,
            attachment_pruning: true,
            overflow_recovery: true,
            compaction_buffer_tokens: 20_000,
            keep_recent_tokens: 20_000,
            retained_user_turns: 0,
            prune_recent_user_turns: 2,
            prune_context_percent: 60,
            token_bytes: std::num::NonZeroUsize::MIN.saturating_add(3),
            attachment_tokens: 1_200,
            recovery_request_overhead_tokens: 512,
            overflow_max_attempts: std::num::NonZeroU32::MIN.saturating_add(2),
            overflow_elide_part_threshold_tokens: 16_000,
            overflow_elided_retained_chars: 400,
            summary_instructions: COMPACTION_PROMPT.into(),
            update_instructions: compaction_update_prompt("{previous_summary}"),
            overflow_instructions: OVERFLOW_RECOVERY_INSTRUCTIONS.into(),
        }
    }

    pub(crate) fn approx_token_count(&self, text: &str) -> usize {
        text.len().div_ceil(self.token_bytes.get())
    }
    pub(crate) fn compaction_threshold(&self, max_context_tokens: usize) -> usize {
        max_context_tokens.saturating_sub(self.compaction_buffer_tokens)
    }
}

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

/// The attachment parts of `messages` older than the recent user turns,
/// back to the latest compaction summary.
fn old_attachment_parts(
    messages: &[Message],
    config: &StandardCompactionConfig,
) -> std::collections::BTreeSet<HistoryPartId> {
    let mut parts = std::collections::BTreeSet::new();
    let mut recent_user_turns = 0usize;
    for message in messages.iter().rev() {
        if is_compaction_summary_message(message) {
            break;
        }
        if message.role == MessageRole::User {
            recent_user_turns += 1;
        }
        if recent_user_turns < config.prune_recent_user_turns {
            continue;
        }
        for (index, part) in message.parts.iter().enumerate() {
            if carries_attachment(part) {
                parts.insert(HistoryPartId {
                    message: message.id.clone(),
                    part: index,
                });
            }
        }
    }
    parts
}

fn carries_attachment(part: &Part) -> bool {
    match part.tool_result_content() {
        Some(blocks) => blocks.iter().any(|block| block.attachment().is_some()),
        None => matches!(part.kind(), PartKind::Attachment) && part.attachment().is_some(),
    }
}

fn strip_all_attachments(messages: &mut [Message], placeholder: &str) -> bool {
    let mut changed = false;
    for message in messages {
        for part in std::sync::Arc::make_mut(&mut message.parts).iter_mut() {
            changed |= omit_part_attachments(part, placeholder);
        }
    }
    changed
}

pub(crate) fn is_compaction_summary_message(message: &Message) -> bool {
    matches!(
        message.origin,
        Some(MessageOrigin::Plugin { ref plugin_id, .. }) if plugin_id == STANDARD_COMPACTION_PLUGIN_ID
    )
}

pub(crate) fn latest_user_index(messages: &[Message]) -> Option<usize> {
    messages
        .iter()
        .rposition(|message| matches!(message.role, MessageRole::User))
}

/// Returns the index of the first message in the "keep" region — everything before it gets
/// The cut always lands on a user-message boundary so we never split a turn.
pub(crate) fn find_compaction_cut_point(
    messages: &[Message],
    prefix_len: usize,
    config: &StandardCompactionConfig,
) -> usize {
    let start = messages[prefix_len..]
        .iter()
        .rposition(is_compaction_summary_message)
        .map(|i| prefix_len + i + 1)
        .unwrap_or(prefix_len);

    let mut accumulated = 0usize;
    for idx in (start..messages.len()).rev() {
        for part in messages[idx].parts.iter() {
            accumulated = accumulated.saturating_add(config.approx_token_count(&part.content()));
            // approximate binary attachment token cost
            accumulated = accumulated.saturating_add(
                config
                    .attachment_tokens
                    .saturating_mul(part.attachments().count()),
            );
        }
        if accumulated >= config.keep_recent_tokens && messages[idx].role == MessageRole::User {
            return idx;
        }
    }
    latest_user_index(messages).unwrap_or(messages.len())
}

/// The one context-pressure fact both standard-compaction decisions consume: a known prompt usage
/// measured against a context window that actually bounds it.  A window of zero bounds nothing,
/// so it carries no pressure at all — the same filter the sans-io section builder applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextPressure {
    used_tokens: usize,
    max_context_tokens: usize,
}

impl ContextPressure {
    fn derive(prompt_usage: Option<&LlmUsage>, max_context_tokens: Option<usize>) -> Option<Self> {
        Some(Self {
            used_tokens: prompt_usage?.total().max(0) as usize,
            max_context_tokens: max_context_tokens.filter(|value| *value > 0)?,
        })
    }

    fn pruning_needed(&self, config: &StandardCompactionConfig) -> bool {
        config.attachment_pruning
            && (self.used_tokens as f64 / self.max_context_tokens as f64)
                >= f64::from(config.prune_context_percent) / 100.0
    }

    fn compaction_needed(&self, config: &StandardCompactionConfig) -> bool {
        config.pressure_compaction
            && self.used_tokens >= config.compaction_threshold(self.max_context_tokens)
    }
}

fn extract_previous_summary(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|m| {
        if !is_compaction_summary_message(m) {
            return None;
        }
        m.parts.first().map(|p| {
            let text = p.content();
            text.strip_prefix(COMPACTION_SUMMARY_TITLE)
                .unwrap_or(&text)
                .trim()
                .to_string()
        })
    })
}

fn append_identity_field(identity: &mut Vec<u8>, value: &str) {
    identity.extend_from_slice(&(value.len() as u64).to_be_bytes());
    identity.extend_from_slice(value.as_bytes());
}

fn latest_physical_turn_id(state: &SessionSnapshot) -> Result<Option<TurnId>, ContextError> {
    let read_view = state.read_view();
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
    token_usage: &'a lash_core::LlmUsage,
    last_prompt_usage: &'a Option<LlmUsage>,
    plugin_config: &'a lash_core::PluginConfig,
    tool_state_ref: &'a Option<lash_core::store::BlobRef>,
    tool_state_generation: Option<u64>,
    plugin_state_ref: &'a Option<lash_core::store::BlobRef>,
    plugin_state_generations: &'a BTreeMap<String, u64>,
    execution_state_ref: &'a Option<lash_core::store::BlobRef>,
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
        plugin_config,
        tool_state_ref,
        tool_state_generation,
        plugin_state_ref,
        plugin_state_generations,
        execution_state_ref,
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
            plugin_config,
            tool_state_ref,
            tool_state_generation: *tool_state_generation,
            plugin_state_ref,
            plugin_state_generations,
            execution_state_ref,
            checkpoint_ref,
        },
        prompt_text,
    };
    let mut identity = serde_json::to_value(identity).map_err(|error| {
        ContextError::Session(format!(
            "failed to encode standard compaction request identity: {error}"
        ))
    })?;
    canonicalize_json_objects(&mut identity);
    serde_json::to_string(&identity).map_err(|error| {
        ContextError::Session(format!(
            "failed to encode standard compaction request identity: {error}"
        ))
    })
}

pub(crate) fn compaction_request_ids(
    parent_session_id: &SessionId,
    state: &SessionSnapshot,
    request_snapshot: &SessionSnapshot,
    prompt_text: &str,
    execution_scope: &lash_core::ExecutionScope,
) -> Result<(SessionId, TurnId), ContextError> {
    let physical_parent_turn_id =
        match latest_physical_turn_id(state)?.or_else(|| execution_scope.turn_id().cloned()) {
            Some(turn_id) => turn_id,
            None => TurnId::parse(execution_scope.id())
                .map_err(|error| ContextError::Session(error.to_string()))?,
        };
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
        LASH_STANDARD_COMPACTION_DOMAIN_VERSION,
        identity,
    );
    Ok((
        parent_session_id.with_suffix(format_args!("-compaction:{discriminator}")),
        physical_parent_turn_id.with_suffix(format_args!(":standard-compaction:{discriminator}")),
    ))
}

pub(crate) fn prepare_compaction_request(
    state: &SessionSnapshot,
    mut prefix_messages: Vec<Message>,
    instructions: Option<&str>,
    config: &StandardCompactionConfig,
) -> Result<(SessionSnapshot, String), ContextError> {
    // The request state only shapes the summarizer's one prompt: it calls no
    // tool and installs no plugin, and it states so.
    let no_tools = lash_core::SessionToolAccess::restricted(std::iter::empty())
        .map_err(|error| ContextError::Session(error.to_string()))?;
    let mut snapshot = lash_core::runtime::RuntimeSessionState::from_snapshot(
        state.clone(),
        lash_core::RuntimeSessionAuthority::new(
            no_tools,
            lash_core::PluginConfig::default(),
            lash_core::prompt_sections::PromptPlan::default(),
        ),
    );
    snapshot.policy.turn_budget = lash_core::TurnBudget::bounded(1);
    strip_all_attachments(&mut prefix_messages, COMPACTED_ATTACHMENT_PLACEHOLDER);
    snapshot.set_execution_state_snapshot(None);
    snapshot.last_prompt_usage = None;
    let previous_summary = extract_previous_summary(&prefix_messages);
    snapshot.replace_active_read_state(&prefix_messages);
    let base_prompt = match previous_summary {
        Some(previous_summary) => config
            .update_instructions
            .replace("{previous_summary}", &previous_summary),
        None => config.summary_instructions.clone(),
    };
    Ok((
        snapshot.to_snapshot(),
        with_instructions(&base_prompt, instructions),
    ))
}

/// The compaction session id and turn id the context-pressure compaction
/// over `ctx` derives for its summarizer request, or `None` when there is
/// nothing to summarize: the same derivation the pressure hook's summary
/// runs, exposed so a replay law can compare what every execution of one
/// turn derives, a redrive from its admitted window included (FIG-4072).
#[doc(hidden)]
pub fn pressure_compaction_request_ids(
    ctx: &ContextPressureContext<'_>,
    config: &StandardCompactionConfig,
) -> Result<Option<(SessionId, TurnId)>, ContextError> {
    let history = ctx.state.messages();
    let summarized = history[leading_system_prefix_len(history)..].to_vec();
    if summarized.is_empty() {
        return Ok(None);
    }
    let state = ctx.state.to_snapshot();
    let (snapshot, prompt_text) = prepare_compaction_request(&state, summarized, None, config)?;
    compaction_request_ids(
        &ctx.session_id,
        &state,
        &snapshot,
        &prompt_text,
        ctx.scoped_effect_controller.execution_scope(),
    )
    .map(Some)
}

/// One direct LLM completion on the parent's own session (FIG-3374).
///
/// The request keeps the durable identities the child-session lane derived:
/// `turn_id` is the replay key, folding the physical parent turn, journal
/// scope, request snapshot, and prompt text, so a redriven parent replays the
/// recorded effect instead of double-billing. `compaction_session_id` survives
/// as the request's `agent_frame_id`.
async fn summarize_compaction_prefix(
    session_id: &SessionId,
    state: &SessionSnapshot,
    prefix_messages: Vec<Message>,
    instructions: Option<&str>,
    config: &StandardCompactionConfig,
    direct_completions: &lash_core::facade_support::DirectCompletionClient<'_>,
    scoped_effect_controller: &lash_core::ActorContext,
) -> Result<Option<String>, ContextError> {
    if prefix_messages.is_empty() {
        return Ok(None);
    }

    let (snapshot, prompt_text) =
        prepare_compaction_request(state, prefix_messages, instructions, config)?;

    let (compaction_session_id, turn_id) = compaction_request_ids(
        session_id,
        state,
        &snapshot,
        &prompt_text,
        scoped_effect_controller.execution_scope(),
    )?;
    let read_view = snapshot.read_view();
    let rendered = lash_sansio::session_model::render_prompt(read_view.messages());

    let model = snapshot.policy.model.as_ref().ok_or_else(|| {
        ContextError::Session("compaction needs the session's model, and it selects none".into())
    })?;
    // The summarizer's instructions are the session's compaction sections,
    // composed into the request when the call is admitted (ADR 0133 §8).
    let request = lash_core::LlmRequest {
        instructions: None,
        model: model.clone(),
        messages: rendered.messages,
        tools: Arc::new(Vec::new()),
        tool_choice: lash_sansio::llm::types::LlmToolChoice::None,
        attachment_acceptance: Arc::clone(&snapshot.policy.attachment_acceptance),
        generation: snapshot.policy.generation.clone(),
        scope: lash_core::LlmRequestScope::new(
            session_id.clone(),
            compaction_session_id.to_string(),
            turn_id.to_string(),
        ),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    };
    let caused_by = scoped_effect_controller
        .execution_scope()
        .turn_id()
        .map(|parent_turn_id| lash_core::CausalRef::Turn {
            session_id: session_id.clone(),
            turn_id: parent_turn_id.clone(),
        });
    let completion = direct_completions
        .direct_llm_completion_for(
            request,
            lash_core::prompt_sections::PromptPurpose::Compaction,
            Some(Arc::new(SummaryInstruction(prompt_text))),
            "compaction",
            caused_by,
        )
        .await
        .map_err(ContextError::from)?;
    match completion.response.terminal_reason {
        lash_sansio::llm::types::LlmTerminalReason::Stop
        | lash_sansio::llm::types::LlmTerminalReason::Unknown => {}
        reason => {
            return Err(ContextError::Pipeline(format!(
                "compaction summary ended with terminal reason `{}` ({}); \
                 refusing to seed a durable frame from an incomplete summary",
                reason.code(),
                completion
                    .response
                    .terminal_diagnostic
                    .as_deref()
                    .unwrap_or("no provider diagnostic"),
            )));
        }
    }
    let summary = completion.response.full_text().trim().to_string();
    if summary.is_empty() {
        return Ok(None);
    }
    Ok(Some(summary))
}

fn compaction_summary_text(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_TITLE}\n{summary}")
}

fn compaction_summary_origin() -> MessageOrigin {
    MessageOrigin::Plugin {
        plugin_id: STANDARD_COMPACTION_PLUGIN_ID.to_string(),
        transient: false,
    }
}

fn compaction_summary_message(summary: &str) -> lash_core::PluginMessage {
    lash_core::PluginMessage::text(MessageRole::Assistant, compaction_summary_text(summary))
        .with_origin(compaction_summary_origin())
}

pub(crate) fn compaction_summary_seed(summary: &str) -> lash_core::SessionAppendNode {
    lash_core::SessionAppendNode::message(compaction_summary_message(summary))
}

fn retained_history_start(messages: &[Message], config: &StandardCompactionConfig) -> usize {
    if config.retained_user_turns == 0 {
        return messages.len();
    }
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.role == MessageRole::User)
        .nth(config.retained_user_turns - 1)
        .map_or(leading_system_prefix_len(messages), |(index, _)| index)
}

fn compaction_seed(summary: &str, retained: &[Message]) -> Vec<lash_core::SessionAppendNode> {
    let mut seed = vec![compaction_summary_seed(summary)];
    seed.extend(retained.iter().map(|message| {
        lash_core::SessionAppendNode::message(lash_core::PluginMessage {
            id: None,
            role: message.role,
            origin: message.origin.clone(),
            parts: message.parts.to_vec(),
        })
    }));
    seed
}

async fn compact_messages_core(
    session_id: &SessionId,
    state: &SessionSnapshot,
    messages: &[Message],
    instructions: Option<&str>,
    config: &StandardCompactionConfig,
    direct_completions: &lash_core::facade_support::DirectCompletionClient<'_>,
    scoped_effect_controller: &lash_core::ActorContext,
) -> Result<Option<ContextCompaction>, ContextError> {
    let prefix_len = leading_system_prefix_len(messages);
    let cut_point = find_compaction_cut_point(messages, prefix_len, config);
    if cut_point <= prefix_len {
        return Ok(None);
    }
    let retained_start = retained_history_start(messages, config);
    if retained_start <= prefix_len {
        return Ok(None);
    }
    let prefix_messages = messages[prefix_len..retained_start].to_vec();
    let Some(summary) = summarize_compaction_prefix(
        session_id,
        state,
        prefix_messages,
        instructions,
        config,
        direct_completions,
        scoped_effect_controller,
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(ContextCompaction::new(compaction_seed(
        &summary,
        &messages[retained_start..],
    ))))
}

pub struct StandardCompactionPluginFactory {
    config: StandardCompactionConfig,
}

impl StandardCompactionPluginFactory {
    pub fn new(config: StandardCompactionConfig) -> Self {
        Self { config }
    }
}

impl Default for StandardCompactionPluginFactory {
    fn default() -> Self {
        Self::new(StandardCompactionConfig::standard())
    }
}

impl PluginFactory for StandardCompactionPluginFactory {
    fn id(&self) -> &'static str {
        STANDARD_COMPACTION_PLUGIN_ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        if self.config.prune_context_percent > 100 {
            return Err(PluginError::Registration(
                "prune_context_percent must be at most 100".into(),
            ));
        }
        Ok(Arc::new(StandardCompactionPlugin {
            config: self.config.clone(),
        }))
    }
}

impl lash_core::plugin::PluginDefinition for StandardCompactionPluginFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(STANDARD_COMPACTION_PLUGIN_ID)
    }
}

struct StandardCompactionPlugin {
    config: StandardCompactionConfig,
}

impl SessionPlugin for StandardCompactionPlugin {
    fn id(&self) -> &'static str {
        STANDARD_COMPACTION_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        use lash_core::plugin::prompt::{
            PromptInput, PromptPlacement, PromptPurpose, PromptRenderError, PromptSectionKey,
            PromptSectionSpec, SectionText,
        };
        reg.prompt().section(
            PromptSectionSpec::new(
                PromptSectionKey::new(SUMMARY_INSTRUCTION_SECTION)
                    .map_err(|error| PluginError::Registration(error.to_string()))?,
                PromptPlacement::CurrentContext,
            )
            .purposes([PromptPurpose::Compaction]),
            Arc::new(|input: &PromptInput<'_>| {
                let instruction =
                    input
                        .protocol_facts::<SummaryInstruction>()
                        .ok_or_else(|| {
                            PromptRenderError::new("compaction summary inputs are absent")
                        })?;
                Ok(SectionText::text(&instruction.0))
            }),
        )?;
        let config = self.config.clone();
        reg.context().pressure(
            100,
            Arc::new(StandardCompactionPressureHook::new(config.clone())),
        )?;
        reg.context().attachment_omissions(
            100,
            Arc::new(StandardCompactionAttachmentPolicy(config.clone())),
        )?;
        reg.context()
            .compact(100, Arc::new(StandardContextCompactor::new(config.clone())))?;
        reg.turn().after(
            lash_core::hook_key!("overflow-recovery"),
            Arc::new(move |ctx: lash_core::plugin::TurnResultHookContext| {
                let config = config.clone();
                Box::pin(async move { overflow_recovery_after_turn(&ctx, &config).await })
            }),
        )?;
        Ok(())
    }
}

/// The durable context policies, decided once per turn before the history
/// policies: a pending context-overflow recovery first (the third
/// context policy), then the context-pressure threshold. Each returns a
/// decision; core writes it and opens the frame (FIG-4110).
struct StandardCompactionPressureHook {
    config: StandardCompactionConfig,
}

impl StandardCompactionPressureHook {
    fn new(config: StandardCompactionConfig) -> Self {
        Self { config }
    }
}

fn turn_trace_context(
    session_id: &SessionId,
    scoped_effect_controller: &lash_core::ActorContext,
) -> lash_core::TraceContext {
    let trace_context = lash_core::TraceContext::default().for_session(session_id.clone());
    match scoped_effect_controller.turn_id() {
        Some(turn_id) => trace_context.for_turn(turn_id),
        None => trace_context,
    }
}

#[async_trait]
impl ContextPressureHook for StandardCompactionPressureHook {
    fn id(&self) -> &'static str {
        "standard_compaction.context_pressure"
    }

    async fn decide(
        &self,
        ctx: &ContextPressureContext<'_>,
    ) -> Result<ContextPressureDecision, ContextError> {
        // Third context policy: recover an unrecovered context overflow
        // before any rolling-pressure decision. The durable marker plus the
        // terminal records say whether recovery is pending; a completed or
        // exhausted record below the marker closes it.
        let recovery_state = OverflowRecoveryState::derive(
            history_recovery_records(&ctx.state).map_err(|error| {
                ContextError::Plugin(PluginError::StoredDataCorrupt {
                    record_kind: error.record_kind,
                    message: error.message,
                })
            })?,
        );
        if self.config.overflow_recovery && recovery_state.pending() {
            return overflow_recovery_decision(ctx, recovery_state, &self.config).await;
        }

        let Some(pressure) =
            ContextPressure::derive(ctx.prompt_usage.as_ref(), ctx.max_context_tokens)
        else {
            return Ok(ContextPressureDecision::Continue);
        };
        if !pressure.compaction_needed(&self.config) {
            return Ok(ContextPressureDecision::Continue);
        }
        ctx.traces.emit(
            turn_trace_context(&ctx.session_id, &ctx.scoped_effect_controller),
            lash_core::TraceEvent::CompactionNeeded {
                used_tokens: pressure.used_tokens,
                max_context_tokens: pressure.max_context_tokens,
                threshold_tokens: self
                    .config
                    .compaction_threshold(pressure.max_context_tokens),
            },
        );

        // FIG-4029: a frame is the context window, so pressure compaction
        // starts one the way an explicit compaction does. The committed frame
        // is summarized, the summary seeds a fresh compaction frame, and this
        // turn runs inside it.
        let history = ctx.state.messages();
        let retained_start = retained_history_start(history, &self.config);
        let summarized = history[leading_system_prefix_len(history)..retained_start].to_vec();
        if summarized.is_empty() {
            return Ok(ContextPressureDecision::Continue);
        }
        let Some(summary) = summarize_compaction_prefix(
            &ctx.session_id,
            &ctx.state.to_snapshot(),
            summarized,
            None,
            &self.config,
            &ctx.direct_completions,
            &ctx.scoped_effect_controller,
        )
        .await?
        else {
            return Ok(ContextPressureDecision::Continue);
        };
        Ok(ContextPressureDecision::OpenFrame {
            records: Vec::new(),
            task: PRESSURE_COMPACTION_TASK.to_string(),
            seed: compaction_seed(&summary, &history[retained_start..]),
        })
    }
}

/// Old-attachment pruning: the one ephemeral policy, an attachment-omission
/// history policy (ADR 0133). It names attachments; it writes nothing.
struct StandardCompactionAttachmentPolicy(StandardCompactionConfig);

impl AttachmentOmissionPolicy for StandardCompactionAttachmentPolicy {
    fn id(&self) -> &'static str {
        "standard_compaction.attachment_omissions"
    }

    fn omissions(
        &self,
        ctx: &AttachmentOmissionContext,
        history: &[Message],
    ) -> Result<std::collections::BTreeSet<HistoryPartId>, PluginError> {
        let Some(pressure) =
            ContextPressure::derive(ctx.prompt_usage.as_ref(), ctx.max_context_tokens)
        else {
            return Ok(Default::default());
        };
        if !pressure.pruning_needed(&self.0) {
            return Ok(Default::default());
        }
        let omissions = old_attachment_parts(history, &self.0);
        if !omissions.is_empty() {
            ctx.traces.emit(
                ctx.trace_context.clone(),
                lash_core::TraceEvent::PromptViewAttachmentsPruned {
                    used_tokens: pressure.used_tokens,
                    max_context_tokens: pressure.max_context_tokens,
                    pruned_attachments: omissions.len(),
                },
            );
        }
        Ok(omissions)
    }
}

struct StandardContextCompactor {
    config: StandardCompactionConfig,
}

impl StandardContextCompactor {
    fn new(config: StandardCompactionConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ContextCompactor for StandardContextCompactor {
    fn id(&self) -> &'static str {
        "standard_compaction.compact"
    }

    async fn compact(
        &self,
        ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError> {
        let trace_context = turn_trace_context(&ctx.session_id, &ctx.scoped_effect_controller);
        ctx.traces.emit(
            trace_context.clone(),
            lash_core::TraceEvent::CompactionStarted {
                source_messages: ctx.state.messages().len(),
                instructions_present: ctx
                    .instructions
                    .as_deref()
                    .is_some_and(|instructions| !instructions.trim().is_empty()),
            },
        );

        let session_id = ctx.session_id.clone();

        let compaction = compact_messages_core(
            &session_id,
            &ctx.state.to_snapshot(),
            ctx.state.messages(),
            ctx.instructions.as_deref(),
            &self.config,
            &ctx.direct_completions,
            &ctx.scoped_effect_controller,
        )
        .await;
        let summary_nodes = compaction
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map_or(0, |compaction| compaction.initial_nodes.len());
        ctx.traces.emit(
            trace_context,
            lash_core::TraceEvent::CompactionCompleted { summary_nodes },
        );
        compaction
    }
}

#[cfg(test)]
mod tests;
