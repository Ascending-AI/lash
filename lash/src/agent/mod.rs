pub(crate) mod exec;
pub mod message;
pub(crate) mod prompt;

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ContextFoldingConfig;
use crate::ExecutionMode;
use crate::ToolDefinition;
use crate::capabilities::AgentCapabilities;
use crate::instructions::{FsInstructionSource, InstructionSource};
use crate::llm::factory::adapter_for;
use crate::llm::types::{LlmOutputPart, LlmRequest, LlmResponse, LlmStreamEvent, LlmToolSpec};
use crate::provider::{OPENAI_GENERIC_DEFAULT_BASE_URL, Provider};
use crate::session::Session;

pub use message::{Message, MessageRole, Part, PartKind, PruneState, messages_to_chat};

#[allow(unused_imports)]
pub(crate) use exec::{ExecAccumulator, execute_and_collect};
#[allow(unused_imports)]
pub(crate) use message::IMAGE_REF_PREFIX;
pub(crate) use prompt::{PromptComposeInput, PromptProfile, compose_system_prompt};
pub use prompt::{PromptOverrideMode, PromptSectionName, PromptSectionOverride};

const CONTEXT_ARCHIVE_MARKER_ID: &str = "__context_archive__";
const MIN_RECENT_USER_TURNS: usize = 3;

/// Send an event to the channel if it's still open.
pub(crate) async fn send_event(tx: &mpsc::Sender<AgentEvent>, event: AgentEvent) {
    if !tx.is_closed() {
        let _ = tx.send(event).await;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ContextFoldResult {
    pub(crate) has_archived_history: bool,
}

fn is_context_archive_marker(msg: &Message) -> bool {
    msg.id == CONTEXT_ARCHIVE_MARKER_ID
}

fn context_archive_marker(history_enabled: bool) -> Message {
    let content = if history_enabled {
        "Earlier turns were archived outside the active context. Use `search_history(...)` when older context matters."
    } else {
        "Earlier turns were archived outside the active context."
    };
    Message {
        id: CONTEXT_ARCHIVE_MARKER_ID.to_string(),
        role: MessageRole::System,
        parts: vec![Part {
            id: format!("{CONTEXT_ARCHIVE_MARKER_ID}.p0"),
            kind: PartKind::Text,
            content: content.to_string(),
            tool_call_id: None,
            tool_name: None,
            prune_state: PruneState::Intact,
        }],
    }
}

fn leading_system_prefix_len(msgs: &[Message]) -> usize {
    msgs.iter()
        .take_while(|msg| msg.role == MessageRole::System)
        .count()
}

fn keep_from_for_recent_turns(msgs: &[Message], prefix_len: usize) -> usize {
    let mut user_turns = 0usize;
    for i in (prefix_len..msgs.len()).rev() {
        if msgs[i].role == MessageRole::User {
            user_turns += 1;
            if user_turns >= MIN_RECENT_USER_TURNS {
                return i;
            }
        }
    }
    prefix_len
}

pub(crate) fn apply_context_folding(
    msgs: &mut Vec<Message>,
    last_input_tokens: usize,
    max_context: usize,
    policy: ContextFoldingConfig,
    history_enabled: bool,
) -> ContextFoldResult {
    let mut result = ContextFoldResult {
        has_archived_history: msgs.iter().any(is_context_archive_marker),
    };
    if last_input_tokens == 0 || msgs.is_empty() {
        return result;
    }

    let hard_budget = max_context * usize::from(policy.hard_limit_pct) / 100;
    if last_input_tokens < hard_budget {
        return result;
    }

    let soft_budget = max_context * usize::from(policy.soft_limit_pct) / 100;
    let prefix_len = leading_system_prefix_len(msgs);
    let total_chars: usize = msgs.iter().map(Message::char_count).sum();
    let target_chars = total_chars.saturating_mul(soft_budget) / last_input_tokens.max(1);

    let mut keep_from = msgs.len();
    let mut tail_chars = 0usize;
    for i in (prefix_len..msgs.len()).rev() {
        let cost = msgs[i].char_count();
        if tail_chars + cost > target_chars {
            break;
        }
        tail_chars += cost;
        keep_from = i;
    }

    keep_from = keep_from.min(keep_from_for_recent_turns(msgs, prefix_len));
    if keep_from <= prefix_len {
        return result;
    }

    msgs.drain(prefix_len..keep_from);
    if !result.has_archived_history {
        msgs.insert(prefix_len, context_archive_marker(history_enabled));
    }
    result.has_archived_history = true;
    result
}

// ─── Token tracking ───

/// Token usage statistics from an LLM call.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
}

impl TokenUsage {
    pub fn total(&self) -> i64 {
        self.input_tokens + self.output_tokens
    }
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
    }
}

/// Configuration for the agent loop.
#[derive(Clone)]
pub struct AgentConfig {
    /// Enabled backend capabilities. Disabled capabilities are omitted from prompt guidance
    /// and not registered in the REPL globals.
    pub capabilities: AgentCapabilities,
    /// Model identifier (e.g. "anthropic/claude-sonnet-4.6")
    pub model: String,
    /// LLM provider (OpenAI-generic, Claude OAuth, Codex, or Google OAuth)
    pub provider: Provider,
    /// Override for context window size (tokens). If None, looked up from model_info.
    pub max_context_tokens: Option<usize>,
    /// When true, use SubAgentStep prompt instead of CodeActStep
    pub sub_agent: bool,
    /// Optional reasoning effort level (e.g. "medium", "high") for Codex
    pub reasoning_effort: Option<String>,
    /// Optional host session ID propagated to provider request metadata.
    pub session_id: Option<String>,
    /// Optional turn limit. None = unlimited (default for root agent).
    pub max_turns: Option<usize>,
    /// When true, include Soul principles in the system prompt.
    pub include_soul: bool,
    /// When set, append raw LLM request/response JSON to this file (debug logging).
    pub llm_log_path: Option<PathBuf>,
    /// When true, use headless prompt (no ask(), no TUI references).
    pub headless: bool,
    /// Ordered prompt section overrides applied on top of the selected profile.
    pub prompt_overrides: Vec<PromptSectionOverride>,
    /// Host-provided instruction source (filesystem by default).
    pub instruction_source: Arc<dyn InstructionSource>,
    /// Execution backend for turns.
    pub execution_mode: ExecutionMode,
    /// Watermark policy for folding old context out of the active prompt window.
    pub context_folding: ContextFoldingConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            capabilities: AgentCapabilities::default(),
            model: "anthropic/claude-sonnet-4.6".to_string(),
            provider: Provider::OpenAiGeneric {
                api_key: String::new(),
                base_url: OPENAI_GENERIC_DEFAULT_BASE_URL.to_string(),
            },
            max_context_tokens: None,
            sub_agent: false,
            reasoning_effort: None,
            session_id: None,
            max_turns: None,
            include_soul: false,
            llm_log_path: None,
            headless: false,
            prompt_overrides: Vec::new(),
            instruction_source: Arc::new(FsInstructionSource::new()),
            execution_mode: crate::default_execution_mode(),
            context_folding: ContextFoldingConfig::default(),
        }
    }
}

/// Standardized error payload surfaced to hosts/UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorEnvelope {
    /// Broad category (e.g. llm_provider, token_refresh, runtime, input_validation).
    pub kind: String,
    /// Optional machine-friendly error code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Human-friendly message safe to show in UI.
    pub user_message: String,
    /// Optional raw/source error text (possibly sanitized/truncated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

/// Low-level events emitted during an agent run.
/// These are intentionally mode-specific; hosts that want a stable cross-mode result should
/// consume `AssembledTurn` from the runtime layer instead.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    #[serde(rename = "text_delta")]
    TextDelta { content: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        name: String,
        args: serde_json::Value,
        result: serde_json::Value,
        success: bool,
        duration_ms: u64,
    },
    #[serde(rename = "code_block")]
    CodeBlock { code: String },
    #[serde(rename = "code_output")]
    CodeOutput {
        output: String,
        error: Option<String>,
    },
    #[serde(rename = "message")]
    Message { text: String, kind: String },
    #[serde(rename = "llm_request")]
    LlmRequest {
        iteration: usize,
        message_count: usize,
        tool_list: String,
    },
    #[serde(rename = "llm_response")]
    LlmResponse {
        iteration: usize,
        content: String,
        duration_ms: u64,
    },
    #[serde(rename = "token_usage")]
    TokenUsage {
        iteration: usize,
        usage: TokenUsage,
        cumulative: TokenUsage,
    },
    #[serde(rename = "retry_status")]
    RetryStatus {
        wait_seconds: u64,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
    #[serde(rename = "sub_agent_done")]
    SubAgentDone {
        task: String,
        usage: TokenUsage,
        tool_calls: usize,
        iterations: usize,
        success: bool,
    },
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ErrorEnvelope>,
    },
    #[serde(rename = "prompt")]
    Prompt {
        question: String,
        options: Vec<String>,
        #[serde(skip)]
        response_tx: std::sync::mpsc::Sender<String>,
    },
}

/// Max retries for rate-limited or empty LLM responses.
pub(crate) const LLM_MAX_RETRIES: usize = 3;
/// Delays between retries (exponential backoff).
pub(crate) const LLM_RETRY_DELAYS: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(10),
];

pub(crate) struct TurnTerminationPolicyState {
    max_steps_final: bool,
    headless_prose_only_streak: usize,
}

impl TurnTerminationPolicyState {
    pub(crate) fn new() -> Self {
        Self {
            max_steps_final: false,
            headless_prose_only_streak: 0,
        }
    }

    pub(crate) fn record_pure_prose_step(&mut self, headless: bool) -> bool {
        if !headless {
            return false;
        }
        self.headless_prose_only_streak += 1;
        self.headless_prose_only_streak >= 3
    }

    pub(crate) fn reset_pure_prose_streak(&mut self) {
        self.headless_prose_only_streak = 0;
    }

    pub(crate) fn should_force_exit_after_grace_turn(&self) -> bool {
        self.max_steps_final
    }

    pub(crate) fn maybe_schedule_turn_limit_final(
        &mut self,
        iteration: usize,
        run_offset: usize,
        max_turns: Option<usize>,
        msgs: &mut Vec<Message>,
    ) {
        let Some(max) = max_turns else { return };
        if iteration < run_offset + max {
            return;
        }
        let sys_id = format!("m{}", msgs.len());
        msgs.push(Message {
            id: sys_id.clone(),
            role: MessageRole::System,
            parts: vec![Part {
                id: format!("{}.p0", sys_id),
                kind: PartKind::Text,
                content: format!(
                    "Turn limit reached ({max}). You MUST call done() now with:\n\
                        1. Summary of what you accomplished\n\
                        2. List of remaining tasks not yet completed\n\
                        3. Recommended next steps\n\
                        Do NOT make any more tool calls. Call done() immediately."
                ),
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            }],
        });
        self.max_steps_final = true;
    }
}

pub(crate) fn make_error_event(
    kind: &str,
    code: Option<&str>,
    user_message: impl Into<String>,
    raw: Option<String>,
) -> AgentEvent {
    let user_message = user_message.into();
    AgentEvent::Error {
        message: user_message.clone(),
        envelope: Some(ErrorEnvelope {
            kind: kind.to_string(),
            code: code.map(str::to_string),
            user_message,
            raw: raw.map(|s| truncate_raw_error(s.trim())),
        }),
    }
}

pub(crate) fn truncate_raw_error(s: &str) -> String {
    const MAX_RAW: usize = 4000;
    if s.len() <= MAX_RAW {
        return s.to_string();
    }
    let keep = MAX_RAW / 2;
    let omitted = s.len() - MAX_RAW;
    format!(
        "{}\n\n... ({omitted} chars omitted) ...\n\n{}",
        &s[..keep],
        &s[s.len() - keep..]
    )
}

pub(crate) fn is_malformed_assistant_output(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if matches!(
        trimmed,
        "<" | "</" | "<repl" | "</repl" | "<repl>" | "</repl>"
    ) {
        return true;
    }

    // Broken or partial REPL tags should never be surfaced as final user text.
    if (trimmed.starts_with("<repl") && !trimmed.contains("</repl>"))
        || trimmed.starts_with("</repl")
        || trimmed.ends_with("<repl")
        || trimmed.ends_with("</repl")
    {
        return true;
    }

    // Catch short dangling tag-like fragments such as "<", "<re", "</r".
    if trimmed.starts_with('<') && !trimmed.contains('>') && trimmed.len() <= 16 {
        let looks_like_fragment = trimmed
            .chars()
            .skip(1)
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '/' || ch == '_' || ch == '-');
        if looks_like_fragment {
            return true;
        }
    }

    false
}

/// CodeAct agent: LLM writes Python code, REPL executes, output feeds back.
pub struct Agent {
    agent_id: String,
    session: Session,
    config: AgentConfig,
    cached_base_context: Option<String>,
}

pub(crate) struct ExecutionPreamble {
    pub(crate) model: String,
    pub(crate) tool_list: String,
    pub(crate) tool_specs: Vec<LlmToolSpec>,
    pub(crate) tool_names: Vec<String>,
    pub(crate) enabled_capability_ids: BTreeSet<String>,
    pub(crate) helper_bindings: BTreeSet<String>,
    pub(crate) capability_prompt_sections: Vec<String>,
    pub(crate) can_write: bool,
    pub(crate) history_enabled: bool,
    #[allow(dead_code)]
    pub(crate) memory_enabled: bool,
    pub(crate) instruction_source: Arc<dyn InstructionSource>,
    pub(crate) project_instructions: String,
    pub(crate) base_context: String,
}

impl Agent {
    pub fn new(session: Session, config: AgentConfig, agent_id: Option<String>) -> Self {
        let agent_id = agent_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Self {
            agent_id,
            session,
            config,
            cached_base_context: None,
        }
    }

    /// Snapshot the REPL session plus scratch files.
    pub async fn snapshot(&mut self) -> Option<Vec<u8>> {
        match self.session.snapshot().await {
            Ok(data) => Some(data),
            Err(e) => {
                tracing::warn!("Session snapshot failed: {}", e);
                None
            }
        }
    }

    /// Restore the REPL session from a snapshot blob.
    pub async fn restore(&mut self, data: &[u8]) -> Result<(), crate::SessionError> {
        self.session.restore(data).await
    }

    pub fn set_model(&mut self, model: String) {
        self.config.model = model;
    }

    pub fn set_provider(&mut self, provider: Provider) {
        self.config.provider = provider;
    }

    pub fn set_reasoning_effort(&mut self, reasoning_effort: Option<String>) {
        self.config.reasoning_effort = reasoning_effort;
    }

    pub fn set_session_id(&mut self, session_id: Option<String>) {
        self.config.session_id = session_id;
    }

    pub fn set_capabilities(&mut self, capabilities: AgentCapabilities) {
        self.config.capabilities = capabilities;
    }

    pub fn set_execution_mode(&mut self, execution_mode: ExecutionMode) {
        self.config.execution_mode = execution_mode;
        self.session.tools().set_execution_mode(execution_mode);
    }

    pub fn set_context_folding(&mut self, context_folding: ContextFoldingConfig) {
        self.config.context_folding = context_folding;
    }

    fn transport_stream_events(
        &self,
        requested: Option<tokio::sync::mpsc::UnboundedSender<LlmStreamEvent>>,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<LlmStreamEvent>> {
        if requested.is_some() {
            return requested;
        }

        let llm = adapter_for(&self.config.provider);
        if llm.requires_streaming() {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<LlmStreamEvent>();
            drop(rx);
            Some(tx)
        } else {
            None
        }
    }

    fn cached_base_context(&mut self) -> String {
        if let Some(context) = self.cached_base_context.as_ref() {
            return context.clone();
        }
        let context = build_context();
        self.cached_base_context = Some(context.clone());
        context
    }

    #[allow(dead_code)]
    fn invalidate_context_cache_if_needed(&mut self, tool_calls: &[crate::ToolCallRecord]) {
        let needs_refresh = tool_calls.iter().any(|call| {
            call.success
                && matches!(
                    call.tool.as_str(),
                    "write_file" | "edit_file" | "find_replace" | "shell"
                )
        });
        if needs_refresh {
            self.cached_base_context = None;
        }
    }

    pub(crate) async fn prepare_execution(
        &mut self,
        mode: ExecutionMode,
    ) -> Result<ExecutionPreamble, AgentEvent> {
        match self.config.provider.ensure_fresh().await {
            Ok(true) => {
                let _ = crate::provider::save_provider(&self.config.provider);
            }
            Err(e) => {
                return Err(make_error_event(
                    "token_refresh",
                    Some("refresh_failed"),
                    format!(
                        "Token refresh failed: {}. Re-authenticate with /provider and retry.",
                        e
                    ),
                    Some(e.to_string()),
                ));
            }
            _ => {}
        }

        let llm = adapter_for(&self.config.provider);
        let model = llm.normalize_model(&self.config.model);

        let all_tools = self.session.tools().definitions();
        let visible: Vec<_> = all_tools.iter().filter(|t| !t.hidden).cloned().collect();
        let prompt_tools: Vec<_> = visible
            .iter()
            .filter(|t| t.inject_into_prompt)
            .cloned()
            .collect();
        let mut tool_list = ToolDefinition::format_tool_docs(&prompt_tools, mode);
        let omitted_tool_count = visible.iter().filter(|t| !t.inject_into_prompt).count();
        if omitted_tool_count > 0 {
            let note = match mode {
                ExecutionMode::Repl => format!(
                    "\n\n- **Note:** {omitted_tool_count} additional tool(s) are available but omitted from this prompt for brevity. Use `list_tools()` / `search_tools(...)` to discover them, then call them directly as global functions."
                ),
                ExecutionMode::NativeTools => {
                    format!(
                        "\n\n- **Note:** {omitted_tool_count} additional tool(s) are available but omitted from this prompt for brevity."
                    )
                }
            };
            tool_list.push_str(&note);
        }
        let tool_specs = if matches!(mode, ExecutionMode::NativeTools) {
            visible
                .iter()
                .map(|tool| LlmToolSpec {
                    name: tool.name.clone(),
                    description: tool.description_for(ExecutionMode::NativeTools),
                    input_schema: tool.input_schema(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let tool_names: Vec<String> = visible.iter().map(|t| t.name.clone()).collect();
        let dynamic_projection = self.session.tools().dynamic_projection();
        let enabled_capability_ids: BTreeSet<String> = dynamic_projection
            .as_ref()
            .map(|p| p.enabled_capabilities.clone())
            .unwrap_or_else(|| {
                self.config
                    .capabilities
                    .enabled_capabilities
                    .iter()
                    .map(|id| id.as_str().to_string())
                    .collect()
            });
        let helper_bindings: BTreeSet<String> = dynamic_projection
            .as_ref()
            .map(|p| p.helper_bindings.clone())
            .unwrap_or_default();
        let capability_prompt_sections: Vec<String> = dynamic_projection
            .as_ref()
            .map(|p| p.prompt_sections.clone())
            .unwrap_or_default();
        let can_write = tool_names
            .iter()
            .any(|name| matches!(name.as_str(), "write_file" | "edit_file" | "find_replace"));
        let history_enabled = helper_bindings.contains("search_history")
            || enabled_capability_ids.contains("history");
        let memory_enabled =
            helper_bindings.contains("search_mem") || enabled_capability_ids.contains("memory");
        let instruction_source = Arc::clone(&self.config.instruction_source);
        let project_instructions = instruction_source.system_instructions();
        let base_context = self.cached_base_context();

        match llm.ensure_ready(&mut self.config.provider).await {
            Ok(changed) => {
                if changed {
                    let _ = crate::provider::save_provider(&self.config.provider);
                }
            }
            Err(e) => {
                return Err(make_error_event(
                    "llm_provider",
                    e.code.as_deref(),
                    format!(
                        "LLM provider initialization failed: {}. Run /provider to reconfigure credentials, then retry.",
                        e.message
                    ),
                    e.raw,
                ));
            }
        }

        Ok(ExecutionPreamble {
            model,
            tool_list,
            tool_specs,
            tool_names,
            enabled_capability_ids,
            helper_bindings,
            capability_prompt_sections,
            can_write,
            history_enabled,
            memory_enabled,
            instruction_source,
            project_instructions,
            base_context,
        })
    }

    pub fn capabilities(&self) -> AgentCapabilities {
        self.config.capabilities.clone()
    }

    pub fn execution_mode(&self) -> ExecutionMode {
        self.config.execution_mode
    }

    /// Reset the underlying Python session (clear namespace).
    pub async fn reset_session(&mut self) -> Result<(), crate::SessionError> {
        self.session.reset().await
    }

    pub async fn reconfigure_session(
        &mut self,
        capabilities_json: String,
        generation: u64,
    ) -> Result<(), crate::SessionError> {
        self.session
            .reconfigure(capabilities_json, generation)
            .await
    }

    /// Run the agent loop with structured messages, emitting events.
    /// Returns the updated message history and the final iteration counter.
    pub async fn run(
        &mut self,
        messages: Vec<Message>,
        images: Vec<Vec<u8>>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
        run_offset: usize,
    ) -> (Vec<Message>, usize) {
        macro_rules! emit {
            ($event:expr) => {
                send_event(&event_tx, $event).await
            };
        }

        if matches!(self.config.execution_mode, ExecutionMode::NativeTools) {
            return self
                .run_native_tools(messages, images, event_tx, cancel, run_offset)
                .await;
        }

        let capabilities_json = self
            .session
            .tools()
            .dynamic_capabilities_payload_json()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "enabled_capabilities": self
                        .config
                        .capabilities
                        .enabled_capabilities
                        .iter()
                        .map(|id| id.as_str())
                        .collect::<Vec<_>>(),
                    "enabled_tools": self
                        .config
                        .capabilities
                        .enabled_tools
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                })
                .to_string()
            });
        let generation = self.session.tools().dynamic_generation().unwrap_or(0);
        if let Err(e) = self
            .session
            .reconfigure(capabilities_json, generation)
            .await
        {
            emit!(make_error_event(
                "tool_projection",
                Some("reconfigure_failed"),
                format!("Failed to refresh REPL tool projection: {e}"),
                Some(e.to_string()),
            ));
            emit!(AgentEvent::Done);
            return (messages, run_offset);
        }

        let preamble = match self.prepare_execution(ExecutionMode::Repl).await {
            Ok(preamble) => preamble,
            Err(event) => {
                emit!(event);
                emit!(AgentEvent::Done);
                return (messages, run_offset);
            }
        };

        let max_context = self
            .config
            .max_context_tokens
            .or_else(|| {
                self.config
                    .provider
                    .context_window(&self.config.model)
                    .map(|v| v as usize)
            })
            .unwrap_or_else(|| {
                eprintln!(
                    "Warning: unknown context window for model '{}', defaulting to 200k",
                    self.config.model
                );
                200_000
            });

        let machine_config = crate::sansio::TurnMachineConfig {
            execution_mode: ExecutionMode::Repl,
            model: preamble.model,
            context_folding: self.config.context_folding,
            max_context_tokens: max_context,
            max_turns: self.config.max_turns,
            headless: self.config.headless,
            sub_agent: self.config.sub_agent,
            include_soul: self.config.include_soul,
            reasoning_effort: self.config.reasoning_effort.clone(),
            session_id: self.config.session_id.clone(),
            tool_list: preamble.tool_list,
            tool_specs: Vec::new(),
            tool_names: preamble.tool_names,
            enabled_capability_ids: preamble.enabled_capability_ids,
            helper_bindings: preamble.helper_bindings,
            capability_prompt_sections: preamble.capability_prompt_sections,
            can_write: preamble.can_write,
            history_enabled: preamble.history_enabled,
            project_instructions: preamble.project_instructions,
            prompt_overrides: self.config.prompt_overrides.clone(),
            base_context: preamble.base_context,
            instruction_source: preamble.instruction_source,
            llm_log_path: self.config.llm_log_path.clone(),
            agent_id: self.agent_id.clone(),
        };

        let mut machine =
            crate::sansio::TurnMachine::new(machine_config, messages, images, run_offset);

        loop {
            let Some(effect) = machine.poll_effect() else {
                break;
            };
            match effect {
                crate::sansio::Effect::Emit(event) => {
                    emit!(event);
                }
                crate::sansio::Effect::Done {
                    messages,
                    iteration,
                } => {
                    return (messages, iteration);
                }
                crate::sansio::Effect::LlmCall { id, request } => {
                    if cancel.is_cancelled() {
                        emit!(AgentEvent::Done);
                        return (Vec::new(), run_offset);
                    }

                    // Set up message forwarding for incremental execution
                    let (msg_tx, mut msg_rx) =
                        tokio::sync::mpsc::unbounded_channel::<crate::SandboxMessage>();
                    self.session.set_message_sender(msg_tx);
                    let event_tx_clone = event_tx.clone();
                    let drain_handle = tokio::spawn(async move {
                        while let Some(sandbox_msg) = msg_rx.recv().await {
                            match sandbox_msg.kind.as_str() {
                                "final" => continue,
                                "tool_output" => {}
                                other => {
                                    tracing::debug!(
                                        "dropping unsupported sandbox message kind: {other}"
                                    );
                                    continue;
                                }
                            }
                            if !event_tx_clone.is_closed() {
                                let _ = event_tx_clone
                                    .send(AgentEvent::Message {
                                        text: sandbox_msg.text,
                                        kind: sandbox_msg.kind,
                                    })
                                    .await;
                            }
                        }
                    });

                    let (prompt_tx, mut prompt_rx) =
                        tokio::sync::mpsc::unbounded_channel::<crate::session::UserPrompt>();
                    self.session.set_prompt_sender(prompt_tx);
                    let event_tx_prompt = event_tx.clone();
                    let prompt_drain_handle = tokio::spawn(async move {
                        while let Some(user_prompt) = prompt_rx.recv().await {
                            if !event_tx_prompt.is_closed() {
                                let _ = event_tx_prompt
                                    .send(AgentEvent::Prompt {
                                        question: user_prompt.question,
                                        options: user_prompt.options,
                                        response_tx: user_prompt.response_tx,
                                    })
                                    .await;
                            }
                        }
                    });

                    // Start streaming LLM call
                    let (llm_stream_tx, mut llm_stream_rx) =
                        tokio::sync::mpsc::unbounded_channel::<LlmStreamEvent>();
                    let llm_request = LlmRequest {
                        stream_events: self.transport_stream_events(Some(llm_stream_tx)),
                        ..request
                    };

                    let mut call_provider = self.config.provider.clone();
                    let llm_task = tokio::spawn(async move {
                        let llm = adapter_for(&call_provider);
                        let result = llm.complete(&mut call_provider, llm_request).await;
                        (result, call_provider)
                    });

                    let mut llm_task = llm_task;
                    let llm_response: Result<LlmResponse, crate::sansio::LlmCallError> = loop {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                llm_task.abort();
                                self.session.clear_message_sender();
                                self.session.clear_prompt_sender();
                                emit!(AgentEvent::Done);
                                return (Vec::new(), run_offset);
                            }
                            maybe_event = llm_stream_rx.recv() => {
                                let Some(event) = maybe_event else { continue };
                                match event {
                                    LlmStreamEvent::Delta(delta) => {
                                        if !machine.handle_llm_delta(id, &delta) {
                                            // Machine wants to cancel stream (fence closed)
                                            llm_task.abort();

                                            // Drain pending exec effects
                                            while let Some(inner_effect) = machine.poll_effect() {
                                                match inner_effect {
                                                    crate::sansio::Effect::Emit(event) => {
                                                        emit!(event);
                                                    }
                                                    crate::sansio::Effect::ExecCode { id: exec_id, code } => {
                                                        let result = self.session.run_code(&code).await;
                                                        let response = match result {
                                                            Ok(r) => crate::sansio::Response::ExecResult {
                                                                id: exec_id,
                                                                result: Ok(r),
                                                            },
                                                            Err(e) => crate::sansio::Response::ExecResult {
                                                                id: exec_id,
                                                                result: Err(e.to_string()),
                                                            },
                                                        };
                                                        machine.handle_response(response);
                                                    }
                                                    crate::sansio::Effect::CancelLlm { .. } => {
                                                        // Already cancelled above
                                                    }
                                                    crate::sansio::Effect::Done { messages, iteration } => {
                                                        self.session.clear_message_sender();
                                                        self.session.clear_prompt_sender();
                                                        let _ = drain_handle.await;
                                                        let _ = prompt_drain_handle.await;
                                                        return (messages, iteration);
                                                    }
                                                    _ => {}
                                                }
                                            }

                                            // Feed a synthetic LlmComplete with stream usage
                                            break Ok(LlmResponse::default());
                                        }
                                    }
                                    LlmStreamEvent::Part(LlmOutputPart::Text { text }) => {
                                        if !machine.handle_llm_delta(id, &text) {
                                            llm_task.abort();
                                            while let Some(inner_effect) = machine.poll_effect() {
                                                match inner_effect {
                                                    crate::sansio::Effect::Emit(event) => {
                                                        emit!(event);
                                                    }
                                                    crate::sansio::Effect::ExecCode { id: exec_id, code } => {
                                                        let result = self.session.run_code(&code).await;
                                                        let response = match result {
                                                            Ok(r) => crate::sansio::Response::ExecResult {
                                                                id: exec_id,
                                                                result: Ok(r),
                                                            },
                                                            Err(e) => crate::sansio::Response::ExecResult {
                                                                id: exec_id,
                                                                result: Err(e.to_string()),
                                                            },
                                                        };
                                                        machine.handle_response(response);
                                                    }
                                                    crate::sansio::Effect::CancelLlm { .. } => {}
                                                    crate::sansio::Effect::Done { messages, iteration } => {
                                                        self.session.clear_message_sender();
                                                        self.session.clear_prompt_sender();
                                                        let _ = drain_handle.await;
                                                        let _ = prompt_drain_handle.await;
                                                        return (messages, iteration);
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            break Ok(LlmResponse::default());
                                        }
                                    }
                                    LlmStreamEvent::Part(LlmOutputPart::ToolCall { .. }) => {}
                                    LlmStreamEvent::Usage(usage) => {
                                        machine.handle_llm_usage(id, &usage);
                                    }
                                }
                            }
                            join = &mut llm_task => {
                                let (result, provider_after) = match join {
                                    Ok(v) => v,
                                    Err(e) => {
                                        break Err(crate::sansio::LlmCallError {
                                            message: format!("internal task failed: {e}"),
                                            retryable: false,
                                            raw: None,
                                            code: Some("task_join_failed".to_string()),
                                        });
                                    }
                                };
                                self.config.provider = provider_after;
                                match result {
                                    Ok(resp) => break Ok(resp),
                                    Err(e) => {
                                        break Err(crate::sansio::LlmCallError {
                                            message: e.message,
                                            retryable: e.retryable,
                                            raw: e.raw,
                                            code: e.code,
                                        });
                                    }
                                }
                            }
                        }
                    };

                    self.session.clear_message_sender();
                    self.session.clear_prompt_sender();
                    let _ = drain_handle.await;
                    let _ = prompt_drain_handle.await;

                    machine.handle_response(crate::sansio::Response::LlmComplete {
                        id,
                        result: llm_response,
                    });
                }
                crate::sansio::Effect::ExecCode { id, code } => {
                    // Set up message forwarding
                    let (msg_tx, mut msg_rx) =
                        tokio::sync::mpsc::unbounded_channel::<crate::SandboxMessage>();
                    self.session.set_message_sender(msg_tx);
                    let event_tx_clone = event_tx.clone();
                    let drain_handle = tokio::spawn(async move {
                        while let Some(sandbox_msg) = msg_rx.recv().await {
                            match sandbox_msg.kind.as_str() {
                                "final" => continue,
                                "tool_output" => {}
                                other => {
                                    tracing::debug!(
                                        "dropping unsupported sandbox message kind: {other}"
                                    );
                                    continue;
                                }
                            }
                            if !event_tx_clone.is_closed() {
                                let _ = event_tx_clone
                                    .send(AgentEvent::Message {
                                        text: sandbox_msg.text,
                                        kind: sandbox_msg.kind,
                                    })
                                    .await;
                            }
                        }
                    });

                    let result = self.session.run_code(&code).await;
                    self.session.clear_message_sender();
                    let _ = drain_handle.await;

                    let response = match result {
                        Ok(r) => crate::sansio::Response::ExecResult { id, result: Ok(r) },
                        Err(e) => crate::sansio::Response::ExecResult {
                            id,
                            result: Err(e.to_string()),
                        },
                    };
                    machine.handle_response(response);
                }
                crate::sansio::Effect::Sleep { id, duration } => {
                    tokio::time::sleep(duration).await;
                    machine.handle_response(crate::sansio::Response::Timeout { id });
                }
                crate::sansio::Effect::CancelLlm { .. } => {
                    // Handled inline during LlmCall processing
                }
                crate::sansio::Effect::ToolCall { .. } => {
                    // REPL mode does not emit native tool calls
                }
            }
        }

        // Machine exhausted without Done — shouldn't happen
        (Vec::new(), run_offset)
    }

    async fn run_native_tools(
        &mut self,
        messages: Vec<Message>,
        images: Vec<Vec<u8>>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
        run_offset: usize,
    ) -> (Vec<Message>, usize) {
        macro_rules! emit {
            ($event:expr) => {
                send_event(&event_tx, $event).await
            };
        }

        let preamble = match self.prepare_execution(ExecutionMode::NativeTools).await {
            Ok(preamble) => preamble,
            Err(event) => {
                emit!(event);
                emit!(AgentEvent::Done);
                return (messages, run_offset);
            }
        };

        let history_enabled = preamble.helper_bindings.contains("search_history")
            || preamble.enabled_capability_ids.contains("history");

        let max_context = self
            .config
            .max_context_tokens
            .or_else(|| {
                self.config
                    .provider
                    .context_window(&self.config.model)
                    .map(|v| v as usize)
            })
            .unwrap_or_else(|| {
                eprintln!(
                    "Warning: unknown context window for model '{}', defaulting to 200k",
                    self.config.model
                );
                200_000
            });

        let machine_config = crate::sansio::TurnMachineConfig {
            execution_mode: ExecutionMode::NativeTools,
            model: preamble.model,
            context_folding: self.config.context_folding,
            max_context_tokens: max_context,
            max_turns: self.config.max_turns,
            headless: self.config.headless,
            sub_agent: self.config.sub_agent,
            include_soul: self.config.include_soul,
            reasoning_effort: self.config.reasoning_effort.clone(),
            session_id: self.config.session_id.clone(),
            tool_list: preamble.tool_list,
            tool_specs: preamble.tool_specs,
            tool_names: preamble.tool_names,
            enabled_capability_ids: preamble.enabled_capability_ids,
            helper_bindings: preamble.helper_bindings,
            capability_prompt_sections: preamble.capability_prompt_sections,
            can_write: preamble.can_write,
            history_enabled,
            project_instructions: preamble.project_instructions,
            prompt_overrides: self.config.prompt_overrides.clone(),
            base_context: preamble.base_context,
            instruction_source: preamble.instruction_source,
            llm_log_path: self.config.llm_log_path.clone(),
            agent_id: self.agent_id.clone(),
        };

        let mut machine =
            crate::sansio::TurnMachine::new(machine_config, messages, images, run_offset);

        // Collect all ToolCall effects to spawn in parallel
        #[allow(dead_code)]
        struct NativeToolOutcome {
            effect_id: crate::sansio::EffectId,
            call_id: String,
            tool_name: String,
            args: serde_json::Value,
            result: crate::ToolResult,
            duration_ms: u64,
        }

        loop {
            let Some(effect) = machine.poll_effect() else {
                break;
            };
            match effect {
                crate::sansio::Effect::Emit(event) => {
                    emit!(event);
                }
                crate::sansio::Effect::Done {
                    messages,
                    iteration,
                } => {
                    return (messages, iteration);
                }
                crate::sansio::Effect::LlmCall { id, request } => {
                    if cancel.is_cancelled() {
                        emit!(AgentEvent::Done);
                        return (Vec::new(), run_offset);
                    }

                    let llm_request = LlmRequest {
                        stream_events: self.transport_stream_events(None),
                        ..request
                    };

                    let mut call_provider = self.config.provider.clone();
                    let llm = adapter_for(&call_provider);
                    let result = llm.complete(&mut call_provider, llm_request).await;
                    self.config.provider = call_provider;

                    let response = match result {
                        Ok(resp) => crate::sansio::Response::LlmComplete {
                            id,
                            result: Ok(resp),
                        },
                        Err(e) => crate::sansio::Response::LlmComplete {
                            id,
                            result: Err(crate::sansio::LlmCallError {
                                message: e.message,
                                retryable: e.retryable,
                                raw: e.raw,
                                code: e.code,
                            }),
                        },
                    };
                    machine.handle_response(response);
                }
                crate::sansio::Effect::ToolCall {
                    id,
                    call_id,
                    tool_name,
                    args,
                } => {
                    // Collect all pending ToolCall effects and execute in parallel
                    let mut pending_tools = vec![(id, call_id, tool_name, args)];
                    while let Some(next) = machine.poll_effect() {
                        match next {
                            crate::sansio::Effect::ToolCall {
                                id,
                                call_id,
                                tool_name,
                                args,
                            } => {
                                pending_tools.push((id, call_id, tool_name, args));
                            }
                            crate::sansio::Effect::Emit(event) => {
                                emit!(event);
                            }
                            other => {
                                // Shouldn't happen — put it back conceptually by handling it
                                // But we can't put it back, so handle inline
                                if let crate::sansio::Effect::Done {
                                    messages,
                                    iteration,
                                } = other
                                {
                                    return (messages, iteration);
                                }
                                break;
                            }
                        }
                    }

                    let tool_provider = Arc::clone(self.session.tools());
                    let mut task_handles = Vec::new();
                    for (eid, call_id, tool_name, mut args) in pending_tools {
                        // Inject catalog for list_tools/search_tools so the caller doesn't need to pass it
                        if (tool_name == "list_tools" || tool_name == "search_tools")
                            && let Some(obj) = args.as_object_mut()
                            && !obj.contains_key("catalog")
                        {
                            let catalog: Vec<serde_json::Value> = tool_provider
                                .definitions()
                                .into_iter()
                                .filter(|d| {
                                    !d.hidden
                                        && !d.description_for(ExecutionMode::NativeTools).is_empty()
                                })
                                .map(|d| {
                                    let p = d.project(ExecutionMode::NativeTools);
                                    serde_json::json!({
                                        "name": p.name,
                                        "description": p.description,
                                        "examples": p.examples,
                                        "inject_into_prompt": p.inject_into_prompt,
                                    })
                                })
                                .collect();
                            obj.insert("catalog".to_string(), serde_json::Value::Array(catalog));
                        }
                        let provider = Arc::clone(&tool_provider);
                        let event_tx_clone = event_tx.clone();
                        let handle = tokio::spawn(async move {
                            let (progress_tx, mut progress_rx) =
                                tokio::sync::mpsc::unbounded_channel::<crate::SandboxMessage>();
                            let progress_event_tx = event_tx_clone.clone();
                            let drain_handle = tokio::spawn(async move {
                                while let Some(sandbox_msg) = progress_rx.recv().await {
                                    if sandbox_msg.kind == "final" {
                                        continue;
                                    }
                                    let _ = progress_event_tx
                                        .send(AgentEvent::Message {
                                            text: sandbox_msg.text,
                                            kind: sandbox_msg.kind,
                                        })
                                        .await;
                                }
                            });

                            let tool_start = std::time::Instant::now();
                            let result = provider
                                .execute_streaming(&tool_name, &args, Some(&progress_tx))
                                .await;
                            drop(progress_tx);
                            let _ = drain_handle.await;

                            NativeToolOutcome {
                                effect_id: eid,
                                call_id,
                                tool_name,
                                args,
                                result,
                                duration_ms: tool_start.elapsed().as_millis() as u64,
                            }
                        });
                        task_handles.push(handle);
                    }

                    for handle in task_handles {
                        let outcome = match handle.await {
                            Ok(outcome) => outcome,
                            Err(e) => NativeToolOutcome {
                                effect_id: crate::sansio::EffectId(0),
                                call_id: uuid::Uuid::new_v4().to_string(),
                                tool_name: "unknown".to_string(),
                                args: serde_json::json!({}),
                                result: crate::ToolResult::err_fmt(format!(
                                    "tool task panicked: {e}"
                                )),
                                duration_ms: 0,
                            },
                        };
                        machine.handle_response(crate::sansio::Response::ToolResult {
                            id: outcome.effect_id,
                            call_id: outcome.call_id,
                            tool_name: outcome.tool_name,
                            result: outcome.result,
                            duration_ms: outcome.duration_ms,
                        });
                    }
                }
                crate::sansio::Effect::Sleep { id, duration } => {
                    tokio::time::sleep(duration).await;
                    machine.handle_response(crate::sansio::Response::Timeout { id });
                }
                crate::sansio::Effect::CancelLlm { .. } => {}
                crate::sansio::Effect::ExecCode { .. } => {
                    // NativeTools mode does not use REPL exec
                }
            }
        }

        (Vec::new(), run_offset)
    }
}

/// Log raw LLM request/response to a debug file (if configured).
pub(crate) fn log_llm_debug(
    log_path: &Option<PathBuf>,
    agent_id: &str,
    iteration: usize,
    usage: &TokenUsage,
    request_body: Option<String>,
    response_text: &str,
) {
    let Some(path) = log_path else { return };

    let entry = serde_json::json!({
        "turn": iteration,
        "ts": chrono::Utc::now().to_rfc3339(),
        "agent_id": agent_id,
        "request": request_body,
        "response": response_text,
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
        }
    });

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", entry);
    }
}

pub(crate) fn format_tool_result_content(success: bool, result: &serde_json::Value) -> String {
    if success {
        match result {
            serde_json::Value::String(text) => text.clone(),
            other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
        }
    } else {
        match result {
            serde_json::Value::String(text) => {
                if text.is_empty() {
                    "[Tool execution failed]".to_string()
                } else {
                    format!("[Tool execution failed]\n{text}")
                }
            }
            other => serde_json::to_string(&serde_json::json!({ "error": other }))
                .unwrap_or_else(|_| "{\"error\":\"tool execution failed\"}".to_string()),
        }
    }
}

/// Build environment context string for the system prompt.
fn build_context() -> String {
    let mut parts = Vec::new();

    if let Ok(cwd) = std::env::current_dir() {
        parts.push(format!("Working directory: {}", cwd.display()));

        let git_dir = cwd.join(".git");
        if git_dir.exists() {
            parts.push("Git repository: yes".into());
        }

        if let Ok(entries) = std::fs::read_dir(&cwd) {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        format!("{}/", name)
                    } else {
                        name
                    }
                })
                .filter(|n| !n.starts_with('.'))
                .collect();
            names.sort();
            if !names.is_empty() {
                parts.push(format!("Top-level entries: {}", names.join(", ")));
            }
        }
    }

    match repl_third_party_packages() {
        Some(pkgs) if pkgs.is_empty() => {
            parts.push("REPL third-party packages: none".into());
        }
        Some(pkgs) => {
            parts.push(format!("REPL third-party packages: {}", pkgs.join(", ")));
        }
        None => {
            parts.push("REPL third-party packages: unknown".into());
        }
    }

    parts.join("\n")
}

/// Monty does not support importing third-party packages inside the REPL.
fn repl_third_party_packages() -> Option<Vec<String>> {
    Some(Vec::new())
}

/// Build alternating Prose/Code parts for an assistant message.
pub(crate) fn build_assistant_parts(
    msg_id: &str,
    prose_parts: &[String],
    code_parts: &[String],
) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut idx = 0;
    let mut prose_iter = prose_parts.iter();
    let mut code_iter = code_parts.iter();

    // Interleave prose and code parts in order they appeared
    // Pattern: prose0, code0, prose1, code1, ...
    // (first prose may be empty if response starts with code)
    loop {
        let prose = prose_iter.next();
        let code = code_iter.next();
        if prose.is_none() && code.is_none() {
            break;
        }
        if let Some(p) = prose
            && !p.is_empty()
        {
            parts.push(Part {
                id: format!("{}.p{}", msg_id, idx),
                kind: PartKind::Prose,
                content: p.clone(),
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            });
            idx += 1;
        }
        if let Some(c) = code
            && !c.is_empty()
        {
            parts.push(Part {
                id: format!("{}.p{}", msg_id, idx),
                kind: PartKind::Code,
                content: c.clone(),
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            });
            idx += 1;
        }
    }

    // If nothing was added (shouldn't happen), add an empty prose part
    if parts.is_empty() {
        parts.push(Part {
            id: format!("{}.p0", msg_id),
            kind: PartKind::Prose,
            content: String::new(),
            tool_call_id: None,
            tool_name: None,
            prune_state: PruneState::Intact,
        });
    }

    parts
}

pub(crate) struct FenceLineParse {
    pub(crate) prose_delta: String,
    pub(crate) codes_to_execute: Vec<String>,
}

pub(crate) fn append_line_segment(target: &mut String, segment: &str, line_started: &mut bool) {
    if segment.is_empty() {
        return;
    }
    if !*line_started {
        if !target.is_empty() {
            target.push('\n');
        }
        *line_started = true;
    }
    target.push_str(segment);
}

/// Parse one streamed line, accepting `<repl>` / `</repl>` tags inline as well as on
/// standalone lines. Returns prose to emit as a text delta and any completed code
/// blocks encountered on that line.
pub(crate) fn parse_fence_line(
    line: &str,
    in_code_fence: &mut bool,
    current_prose: &mut String,
    current_code: &mut String,
    prose_parts: &mut Vec<String>,
) -> FenceLineParse {
    const OPEN_TAG: &str = "<repl>";
    const CLOSE_TAG: &str = "</repl>";

    let mut out = FenceLineParse {
        prose_delta: String::new(),
        codes_to_execute: Vec::new(),
    };

    let mut remaining = line;
    let mut prose_started_this_line = false;
    let mut code_started_this_line = false;

    // Preserve intentional blank lines inside code fences (e.g. triple-quoted
    // markdown passed to done(...)). Without this, markdown paragraphs collapse.
    if *in_code_fence && line.is_empty() && !current_code.is_empty() {
        current_code.push('\n');
        return out;
    }

    loop {
        if !*in_code_fence {
            if let Some(idx) = remaining.find(OPEN_TAG) {
                let before = &remaining[..idx];
                append_line_segment(current_prose, before, &mut prose_started_this_line);
                out.prose_delta.push_str(before);

                // Opening fence: flush prose accumulated so far into prose parts.
                let prose = current_prose.trim().to_string();
                if !prose.is_empty() {
                    prose_parts.push(prose);
                }
                current_prose.clear();
                *in_code_fence = true;
                current_code.clear();
                code_started_this_line = false;
                remaining = &remaining[idx + OPEN_TAG.len()..];
                continue;
            }

            append_line_segment(current_prose, remaining, &mut prose_started_this_line);
            out.prose_delta.push_str(remaining);
            break;
        }

        if let Some(idx) = remaining.find(CLOSE_TAG) {
            let before = &remaining[..idx];
            append_line_segment(current_code, before, &mut code_started_this_line);
            *in_code_fence = false;
            let code = std::mem::take(current_code);
            remaining = &remaining[idx + CLOSE_TAG.len()..];
            code_started_this_line = false;
            if !code.trim().is_empty() {
                out.codes_to_execute.push(code);
            }
            continue;
        }

        append_line_segment(current_code, remaining, &mut code_started_this_line);
        break;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{
        apply_context_folding, format_tool_result_content, is_context_archive_marker,
        is_malformed_assistant_output, parse_fence_line,
    };
    use crate::ContextFoldingConfig;
    use crate::agent::{Message, MessageRole, Part, PartKind, PruneState};

    fn text_message(id: &str, role: MessageRole, content: &str) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: vec![Part {
                id: format!("{id}.p0"),
                kind: PartKind::Text,
                content: content.to_string(),
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            }],
        }
    }

    #[test]
    fn parses_inline_open_and_close_tags() {
        let mut in_code_fence = false;
        let mut current_prose = String::new();
        let mut current_code = String::new();
        let mut prose_parts = Vec::new();

        let out = parse_fence_line(
            "preface <repl>print('x')</repl>",
            &mut in_code_fence,
            &mut current_prose,
            &mut current_code,
            &mut prose_parts,
        );

        assert_eq!(out.prose_delta, "preface ");
        assert_eq!(out.codes_to_execute, vec!["print('x')"]);
        assert_eq!(prose_parts, vec!["preface"]);
        assert!(!in_code_fence);
        assert!(current_prose.is_empty());
        assert!(current_code.is_empty());
    }

    #[test]
    fn parses_inline_open_tag_without_close() {
        let mut in_code_fence = false;
        let mut current_prose = String::new();
        let mut current_code = String::new();
        let mut prose_parts = Vec::new();

        let out = parse_fence_line(
            "note<repl>x = 1",
            &mut in_code_fence,
            &mut current_prose,
            &mut current_code,
            &mut prose_parts,
        );

        assert_eq!(out.prose_delta, "note");
        assert!(out.codes_to_execute.is_empty());
        assert_eq!(prose_parts, vec!["note"]);
        assert!(in_code_fence);
        assert_eq!(current_code, "x = 1");
    }

    #[test]
    fn close_tag_executes_accumulated_multiline_code() {
        let mut in_code_fence = true;
        let mut current_prose = String::new();
        let mut current_code = "x = 1".to_string();
        let mut prose_parts = Vec::new();

        let out = parse_fence_line(
            "print(x)</repl> trailing text",
            &mut in_code_fence,
            &mut current_prose,
            &mut current_code,
            &mut prose_parts,
        );

        assert_eq!(out.codes_to_execute, vec!["x = 1\nprint(x)"]);
        assert_eq!(out.prose_delta, " trailing text");
        assert!(!in_code_fence);
        assert!(current_code.is_empty());
    }

    #[test]
    fn parses_multiple_inline_blocks_on_same_line() {
        let mut in_code_fence = false;
        let mut current_prose = String::new();
        let mut current_code = String::new();
        let mut prose_parts = Vec::new();

        let out = parse_fence_line(
            "<repl>a=1</repl><repl>b=2</repl>",
            &mut in_code_fence,
            &mut current_prose,
            &mut current_code,
            &mut prose_parts,
        );

        assert!(out.prose_delta.is_empty());
        assert_eq!(out.codes_to_execute, vec!["a=1", "b=2"]);
        assert!(!in_code_fence);
    }

    #[test]
    fn preserves_blank_lines_inside_code_block() {
        let mut in_code_fence = true;
        let mut current_prose = String::new();
        let mut current_code = String::new();
        let mut prose_parts = Vec::new();

        let _ = parse_fence_line(
            "a = 1",
            &mut in_code_fence,
            &mut current_prose,
            &mut current_code,
            &mut prose_parts,
        );
        let _ = parse_fence_line(
            "",
            &mut in_code_fence,
            &mut current_prose,
            &mut current_code,
            &mut prose_parts,
        );
        let out = parse_fence_line(
            "b = 2</repl>",
            &mut in_code_fence,
            &mut current_prose,
            &mut current_code,
            &mut prose_parts,
        );

        assert_eq!(out.codes_to_execute, vec!["a = 1\n\nb = 2"]);
        assert!(!in_code_fence);
    }

    #[test]
    fn malformed_output_detector_flags_partial_repl_fragments() {
        assert!(is_malformed_assistant_output("<"));
        assert!(is_malformed_assistant_output("<re"));
        assert!(is_malformed_assistant_output("</r"));
        assert!(is_malformed_assistant_output("<repl"));
        assert!(is_malformed_assistant_output("</repl"));
        assert!(is_malformed_assistant_output("<repl>"));
        assert!(!is_malformed_assistant_output("All good here."));
        assert!(!is_malformed_assistant_output("<tag>"));
        assert!(!is_malformed_assistant_output(
            "Use `<repl>` tags only when needed."
        ));
    }

    #[test]
    fn formats_successful_tool_results_without_wrapper() {
        assert_eq!(
            format_tool_result_content(true, &serde_json::json!("done")),
            "done"
        );
        assert_eq!(
            format_tool_result_content(true, &serde_json::json!({"count": 2})),
            r#"{"count":2}"#
        );
    }

    #[test]
    fn formats_failed_tool_results_as_explicit_errors() {
        assert_eq!(
            format_tool_result_content(false, &serde_json::json!("permission denied")),
            "[Tool execution failed]\npermission denied"
        );
        assert_eq!(
            format_tool_result_content(false, &serde_json::json!({"code": "ENOENT"})),
            r#"{"error":{"code":"ENOENT"}}"#
        );
    }

    #[test]
    fn context_folding_inserts_single_archive_marker_and_keeps_recent_turns() {
        let mut msgs = vec![
            text_message("u1", MessageRole::User, &"u1".repeat(40)),
            text_message("a1", MessageRole::Assistant, &"a1".repeat(40)),
            text_message("u2", MessageRole::User, &"u2".repeat(40)),
            text_message("a2", MessageRole::Assistant, &"a2".repeat(40)),
            text_message("u3", MessageRole::User, &"u3".repeat(40)),
            text_message("a3", MessageRole::Assistant, &"a3".repeat(40)),
            text_message("u4", MessageRole::User, &"u4".repeat(40)),
            text_message("a4", MessageRole::Assistant, &"a4".repeat(40)),
        ];

        let result =
            apply_context_folding(&mut msgs, 70, 100, ContextFoldingConfig::default(), true);

        assert!(result.has_archived_history);
        assert!(is_context_archive_marker(&msgs[0]));
        assert_eq!(
            msgs.iter()
                .filter(|msg| is_context_archive_marker(msg))
                .count(),
            1
        );
        assert!(!msgs.iter().any(|msg| msg.id == "u1"));
        assert!(msgs.iter().any(|msg| msg.id == "u2"));
        assert!(msgs.iter().any(|msg| msg.id == "u4"));
    }

    #[test]
    fn context_folding_does_not_repeat_between_soft_and_hard() {
        let mut msgs = vec![
            text_message("__context_archive__", MessageRole::System, "archived"),
            text_message("u1", MessageRole::User, "hello"),
            text_message("a1", MessageRole::Assistant, "world"),
        ];

        let result =
            apply_context_folding(&mut msgs, 55, 100, ContextFoldingConfig::default(), true);

        assert!(result.has_archived_history);
        assert_eq!(
            msgs.iter()
                .filter(|msg| is_context_archive_marker(msg))
                .count(),
            1
        );
    }
}

/// Resolve and aggregate context-aware instructions discovered during this turn.
/// We currently trigger this on successful `read_file` tool calls with a `path` argument.
pub(crate) fn resolve_context_instructions(
    source: &dyn InstructionSource,
    tool_calls: &[crate::ToolCallRecord],
) -> String {
    let mut seen_paths = HashSet::new();
    let read_paths: Vec<String> = tool_calls
        .iter()
        .filter(|tc| tc.success && tc.tool == "read_file")
        .filter_map(|tc| tc.args.get("path").and_then(|v| v.as_str()))
        .filter(|p| !p.is_empty())
        .filter(|p| seen_paths.insert((*p).to_string()))
        .map(str::to_string)
        .collect();
    source.context_instructions_for_reads(&read_paths)
}
