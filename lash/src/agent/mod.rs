mod exec;
pub mod message;
mod prompt;

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
use crate::llm::types::{
    LlmAttachment, LlmOutputPart, LlmRequest, LlmResponse, LlmStreamEvent, LlmToolChoice,
    LlmToolSpec, LlmUsage,
};
use crate::provider::{OPENAI_GENERIC_DEFAULT_BASE_URL, Provider};
use crate::session::Session;

pub use message::{Message, MessageRole, Part, PartKind, PruneState, messages_to_chat};

use exec::{ExecAccumulator, execute_and_collect};
use message::IMAGE_REF_PREFIX;
use prompt::{PromptComposeInput, PromptProfile, compose_system_prompt};
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
struct ContextFoldResult {
    has_archived_history: bool,
}

fn is_context_archive_marker(msg: &Message) -> bool {
    msg.id == CONTEXT_ARCHIVE_MARKER_ID
}

fn context_archive_marker(history_enabled: bool) -> Message {
    let content = if history_enabled {
        "Earlier turns were archived outside the active context. Use `_history.user_messages()`, `_history.search(...)`, or `_history[i]` for full-fidelity recall."
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

fn apply_context_folding(
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
            execution_mode: ExecutionMode::Repl,
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
const LLM_MAX_RETRIES: usize = 3;
/// Delays between retries (exponential backoff).
const LLM_RETRY_DELAYS: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(10),
];

struct TurnTerminationPolicyState {
    max_steps_final: bool,
    headless_prose_only_streak: usize,
}

impl TurnTerminationPolicyState {
    fn new() -> Self {
        Self {
            max_steps_final: false,
            headless_prose_only_streak: 0,
        }
    }

    fn record_pure_prose_step(&mut self, headless: bool) -> bool {
        if !headless {
            return false;
        }
        self.headless_prose_only_streak += 1;
        self.headless_prose_only_streak >= 3
    }

    fn reset_pure_prose_streak(&mut self) {
        self.headless_prose_only_streak = 0;
    }

    fn should_force_exit_after_grace_turn(&self) -> bool {
        self.max_steps_final
    }

    fn maybe_schedule_turn_limit_final(
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

fn make_error_event(
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

fn truncate_raw_error(s: &str) -> String {
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

fn is_malformed_assistant_output(text: &str) -> bool {
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

struct ExecutionPreamble {
    model: String,
    tool_list: String,
    tool_specs: Vec<LlmToolSpec>,
    tool_names: Vec<String>,
    enabled_capability_ids: BTreeSet<String>,
    helper_bindings: BTreeSet<String>,
    capability_prompt_sections: Vec<String>,
    can_write: bool,
    history_enabled: bool,
    memory_enabled: bool,
    instruction_source: Arc<dyn InstructionSource>,
    project_instructions: String,
    base_context: String,
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

    /// Snapshot the Python session (dill + scratch files).
    pub async fn snapshot(&mut self) -> Option<Vec<u8>> {
        match self.session.snapshot().await {
            Ok(data) => Some(data),
            Err(e) => {
                tracing::warn!("Session snapshot failed: {}", e);
                None
            }
        }
    }

    /// Restore the Python session from a snapshot blob.
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

    fn cached_base_context(&mut self) -> String {
        if let Some(context) = self.cached_base_context.as_ref() {
            return context.clone();
        }
        let context = build_context();
        self.cached_base_context = Some(context.clone());
        context
    }

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

    async fn prepare_execution(
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
                    "\n\n- **Note:** {omitted_tool_count} additional tool(s) are available but omitted from this prompt for brevity. Use `list_tools()` / `search_tools(...)` to discover them, then call via `tools.<tool>(...)`."
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

        let ExecutionPreamble {
            model,
            tool_list,
            tool_specs: _,
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
        } = match self.prepare_execution(ExecutionMode::Repl).await {
            Ok(preamble) => preamble,
            Err(event) => {
                emit!(event);
                emit!(AgentEvent::Done);
                return (messages, run_offset);
            }
        };

        // Keep raw image bytes so provider-specific transports can materialize
        // multimodal payloads in their native wire format.
        let user_images: Vec<(String, Vec<u8>)> = images
            .into_iter()
            .map(|png_bytes| ("image/png".to_string(), png_bytes))
            .collect();

        let mut cumulative_usage = TokenUsage::default();
        let mut last_input_tokens: usize = 0;

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

        let mut msgs = messages;
        let mut iteration: usize = run_offset;
        let mut tool_images: Vec<(String, Vec<u8>)> = Vec::new();
        let mut termination = TurnTerminationPolicyState::new();

        loop {
            if cancel.is_cancelled() {
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }

            let fold = apply_context_folding(
                &mut msgs,
                last_input_tokens,
                max_context,
                self.config.context_folding,
                history_enabled,
            );
            let has_history = fold.has_archived_history;
            let context = base_context.clone();

            let chat_msgs = messages_to_chat(&msgs);

            emit!(AgentEvent::LlmRequest {
                iteration,
                message_count: msgs.len(),
                tool_list: tool_list.clone(),
            });

            // Set up message forwarding for incremental execution
            let (msg_tx, mut msg_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::SandboxMessage>();
            self.session.set_message_sender(msg_tx);
            let event_tx_clone = event_tx.clone();
            let drain_handle = tokio::spawn(async move {
                while let Some(sandbox_msg) = msg_rx.recv().await {
                    // Explicit allowlist for sandbox message kinds rendered in the TUI.
                    // Unknown kinds are dropped by default so random tool/runtime chatter
                    // never leaks into user-visible output.
                    match sandbox_msg.kind.as_str() {
                        "final" => continue,
                        "tool_output" => {}
                        other => {
                            tracing::debug!("dropping unsupported sandbox message kind: {other}");
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

            // Execution state for fence-aware parsing
            let mut acc = ExecAccumulator::new();
            let mut response = String::new();
            let mut prose_parts: Vec<String> = Vec::new();
            let mut code_parts: Vec<String> = Vec::new();
            // Fence parser state
            let mut in_code_fence = false;
            let mut current_prose = String::new();
            let mut current_code = String::new();
            let mut last_line_start = 0usize; // byte offset of current incomplete line
            let mut code_executed = false;
            let mut direct_usage: Option<(TokenUsage, Option<String>, Option<String>)> = None;
            let mut malformed_terminal_failure = false;

            // LLM call with retry on transient API errors
            let full_text = 'llm_retry: {
                let mut last_error = None;
                for attempt in 0..=LLM_MAX_RETRIES {
                    if attempt > 0 {
                        let delay = LLM_RETRY_DELAYS[attempt - 1];
                        let reason = last_error
                            .clone()
                            .unwrap_or_else(|| "transient provider error".to_string());
                        tracing::warn!(
                            "Retrying LLM call (attempt {}/{}) after {}s",
                            attempt + 1,
                            LLM_MAX_RETRIES + 1,
                            delay.as_secs()
                        );
                        emit!(AgentEvent::RetryStatus {
                            wait_seconds: delay.as_secs(),
                            attempt: attempt + 1,
                            max_attempts: LLM_MAX_RETRIES + 1,
                            reason,
                        });
                        tokio::time::sleep(delay).await;
                    }

                    let llm_start = std::time::Instant::now();

                    let all_images: Vec<(String, Vec<u8>)> = user_images
                        .iter()
                        .chain(tool_images.iter())
                        .map(|(mime, data)| (mime.clone(), data.clone()))
                        .collect();
                    tool_images.clear();

                    let include_soul = if self.config.sub_agent {
                        self.config.include_soul
                    } else {
                        true
                    };
                    let profile =
                        PromptProfile::from_flags(self.config.headless, self.config.sub_agent);
                    let system_prompt = compose_system_prompt(PromptComposeInput {
                        profile,
                        execution_mode: self.config.execution_mode,
                        context: &context,
                        tool_list: &tool_list,
                        tool_names: &tool_names,
                        has_history,
                        enabled_capability_ids: &enabled_capability_ids,
                        helper_bindings: &helper_bindings,
                        capability_prompt_sections: &capability_prompt_sections,
                        can_write,
                        include_soul,
                        project_instructions: &project_instructions,
                        overrides: &self.config.prompt_overrides,
                    });

                    let llm_request = LlmRequest {
                        model: model.clone(),
                        system_prompt: system_prompt.clone(),
                        messages: chat_msgs.clone(),
                        attachments: all_images
                            .iter()
                            .map(|(mime, data)| LlmAttachment {
                                mime: mime.clone(),
                                data: data.clone(),
                            })
                            .collect(),
                        tools: Vec::new(),
                        tool_choice: LlmToolChoice::None,
                        reasoning_effort: self.config.reasoning_effort.clone(),
                        session_id: self.config.session_id.clone(),
                        stream_events: None,
                    };

                    tracing::info!(
                        "LLM turn {} start: model={} messages={} attachments={}",
                        iteration,
                        model,
                        llm_request.messages.len(),
                        llm_request.attachments.len()
                    );

                    let (llm_stream_tx, mut llm_stream_rx) =
                        tokio::sync::mpsc::unbounded_channel::<LlmStreamEvent>();
                    let llm_request = LlmRequest {
                        stream_events: Some(llm_stream_tx),
                        ..llm_request
                    };

                    let mut call_provider = self.config.provider.clone();
                    let llm_task = tokio::spawn(async move {
                        let llm = adapter_for(&call_provider);
                        let result = llm.complete(&mut call_provider, llm_request).await;
                        (result, call_provider)
                    });

                    // Turn semantics: a single model turn may contain prose or one REPL block.
                    // After the first completed REPL block we stop consuming this completion,
                    // reinject execution output, and ask the model for the next step.
                    let break_on_first_code = true;
                    let mut streamed_delta_count = 0usize;
                    let mut stream_event_count = 0usize;
                    let mut usage_event_count = 0usize;
                    let mut streamed_char_count = 0usize;
                    let mut latest_stream_usage = LlmUsage::default();
                    let mut stop_stream_processing = false;
                    let mut retry_after_error = false;
                    let mut wait_tick = tokio::time::interval(std::time::Duration::from_secs(15));
                    wait_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    // Consume the immediate first tick so periodic logs start after 15s.
                    let _ = wait_tick.tick().await;
                    let mut llm_task = llm_task;
                    let llm_response: LlmResponse = loop {
                        if stop_stream_processing
                            && ((break_on_first_code && code_executed)
                                || !acc.final_response.is_empty())
                        {
                            tracing::info!(
                                "LLM turn {} cutting stream early after code execution (stream_events={} deltas={} delta_chars={} usage_events={}); reinjecting outputs",
                                iteration,
                                stream_event_count,
                                streamed_delta_count,
                                streamed_char_count,
                                usage_event_count
                            );
                            llm_task.abort();
                            break LlmResponse {
                                usage: latest_stream_usage.clone(),
                                ..LlmResponse::default()
                            };
                        }
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                llm_task.abort();
                                self.session.clear_message_sender();
                                self.session.clear_prompt_sender();
                                emit!(AgentEvent::Done);
                                return (msgs, iteration);
                            }
                            _ = wait_tick.tick() => {
                                tracing::info!(
                                    "LLM turn {} waiting: elapsed={}s stream_events={} deltas={} delta_chars={} usage_events={} stop_stream={} code_executed={}",
                                    iteration,
                                    llm_start.elapsed().as_secs(),
                                    stream_event_count,
                                    streamed_delta_count,
                                    streamed_char_count,
                                    usage_event_count,
                                    stop_stream_processing,
                                    code_executed,
                                );
                            }
                            maybe_event = llm_stream_rx.recv() => {
                                let Some(event) = maybe_event else { continue };
                                stream_event_count += 1;
                                match event {
                                    LlmStreamEvent::Delta(delta) => {
                                        if stop_stream_processing || delta.is_empty() {
                                            continue;
                                        }
                                        streamed_delta_count += 1;
                                        streamed_char_count += delta.len();
                                        response.push_str(&delta);

                                        while let Some(nl) = response[last_line_start..].find('\n') {
                                            let line_end = last_line_start + nl;
                                            let line = response[last_line_start..line_end].to_string();
                                            last_line_start = line_end + 1;

                                            let parsed = parse_fence_line(
                                                &line,
                                                &mut in_code_fence,
                                                &mut current_prose,
                                                &mut current_code,
                                                &mut prose_parts,
                                            );

                                            if !parsed.prose_delta.is_empty() {
                                                emit!(AgentEvent::TextDelta {
                                                    content: format!("{}\n", parsed.prose_delta),
                                                });
                                            }

                                            for code in parsed.codes_to_execute {
                                                code_parts.push(code.clone());
                                                emit!(AgentEvent::CodeBlock { code: code.clone() });
                                                if !acc.had_failure {
                                                    execute_and_collect(
                                                        &mut self.session,
                                                        &code,
                                                        &mut acc,
                                                        &event_tx,
                                                    )
                                                    .await;
                                                }
                                                code_executed = true;
                                                if break_on_first_code || !acc.final_response.is_empty() {
                                                    stop_stream_processing = true;
                                                    break;
                                                }
                                            }
                                            if stop_stream_processing {
                                                break;
                                            }
                                        }
                                        if (break_on_first_code && code_executed)
                                            || !acc.final_response.is_empty()
                                        {
                                            stop_stream_processing = true;
                                        }
                                    }
                                    LlmStreamEvent::Part(part) => match part {
                                        LlmOutputPart::Text { text } => {
                                            if stop_stream_processing || text.is_empty() {
                                                continue;
                                            }
                                            streamed_delta_count += 1;
                                            streamed_char_count += text.len();
                                            response.push_str(&text);

                                            while let Some(nl) = response[last_line_start..].find('\n') {
                                                let line_end = last_line_start + nl;
                                                let line = response[last_line_start..line_end].to_string();
                                                last_line_start = line_end + 1;

                                                let parsed = parse_fence_line(
                                                    &line,
                                                    &mut in_code_fence,
                                                    &mut current_prose,
                                                    &mut current_code,
                                                    &mut prose_parts,
                                                );

                                                if !parsed.prose_delta.is_empty() {
                                                    emit!(AgentEvent::TextDelta {
                                                        content: format!("{}\n", parsed.prose_delta),
                                                    });
                                                }

                                                for code in parsed.codes_to_execute {
                                                    code_parts.push(code.clone());
                                                    emit!(AgentEvent::CodeBlock { code: code.clone() });
                                                    if !acc.had_failure {
                                                        execute_and_collect(
                                                            &mut self.session,
                                                            &code,
                                                            &mut acc,
                                                            &event_tx,
                                                        )
                                                        .await;
                                                    }
                                                    code_executed = true;
                                                    if break_on_first_code || !acc.final_response.is_empty() {
                                                        stop_stream_processing = true;
                                                        break;
                                                    }
                                                }
                                                if stop_stream_processing {
                                                    break;
                                                }
                                            }
                                            if (break_on_first_code && code_executed)
                                                || !acc.final_response.is_empty()
                                            {
                                                stop_stream_processing = true;
                                            }
                                        }
                                        LlmOutputPart::ToolCall { .. } => {
                                            // Native tool-calling responses are handled by the
                                            // dedicated native-tools loop. Ignore them here so
                                            // the legacy REPL path stays stable.
                                        }
                                    },
                                    LlmStreamEvent::Usage(usage) => {
                                        usage_event_count += 1;
                                        latest_stream_usage = usage.clone();
                                        tracing::debug!(
                                            "LLM turn {} usage event: in={} out={} cached={} (stream_events={})",
                                            iteration,
                                            usage.input_tokens,
                                            usage.output_tokens,
                                            usage.cached_input_tokens,
                                            stream_event_count
                                        );
                                    }
                                }
                            }
                            join = &mut llm_task => {
                                tracing::info!(
                                    "LLM turn {} transport finished after {}ms (stream_events={} deltas={} delta_chars={} usage_events={})",
                                    iteration,
                                    llm_start.elapsed().as_millis(),
                                    stream_event_count,
                                    streamed_delta_count,
                                    streamed_char_count,
                                    usage_event_count
                                );
                                let (result, provider_after) = match join {
                                    Ok(v) => v,
                                    Err(e) => {
                                        emit!(AgentEvent::Error {
                                            message: format!("LLM error: internal task failed: {e}"),
                                            envelope: Some(ErrorEnvelope {
                                                kind: "llm_provider".to_string(),
                                                code: Some("task_join_failed".to_string()),
                                                user_message: format!("LLM error: internal task failed: {e}"),
                                                raw: None,
                                            }),
                                        });
                                        break 'llm_retry String::new();
                                    }
                                };
                                self.config.provider = provider_after;
                                match result {
                                    Ok(resp) => break resp,
                                    Err(e) => {
                                        tracing::warn!(
                                            "LLM turn {} transport error after {}ms: {} (retryable={})",
                                            iteration,
                                            llm_start.elapsed().as_millis(),
                                            e.message,
                                            e.retryable
                                        );
                                        if e.retryable && attempt < LLM_MAX_RETRIES {
                                            last_error = Some(e.message);
                                            retry_after_error = true;
                                            break LlmResponse::default();
                                        }
                                        emit!(AgentEvent::Error {
                                            message: format!("LLM error: {}", e.message),
                                            envelope: Some(ErrorEnvelope {
                                                kind: "llm_provider".to_string(),
                                                code: e.code.clone(),
                                                user_message: format!("LLM error: {}", e.message),
                                                raw: e.raw.clone().map(|s| truncate_raw_error(&s)),
                                            }),
                                        });
                                        break 'llm_retry String::new();
                                    }
                                }
                            }
                        }
                    };
                    if retry_after_error {
                        continue;
                    }

                    direct_usage = Some((
                        TokenUsage {
                            input_tokens: llm_response.usage.input_tokens,
                            output_tokens: llm_response.usage.output_tokens,
                            cached_input_tokens: llm_response.usage.cached_input_tokens,
                        },
                        llm_response.request_body.clone(),
                        llm_response.http_summary.clone(),
                    ));
                    tracing::info!(
                        "LLM turn {} usage final: in={} out={} cached={}",
                        iteration,
                        llm_response.usage.input_tokens,
                        llm_response.usage.output_tokens,
                        llm_response.usage.cached_input_tokens
                    );

                    if streamed_delta_count == 0 {
                        for delta in &llm_response.deltas {
                            if cancel.is_cancelled() {
                                self.session.clear_message_sender();
                                self.session.clear_prompt_sender();
                                emit!(AgentEvent::Done);
                                return (msgs, iteration);
                            }

                            if delta.is_empty() {
                                continue;
                            }
                            response.push_str(delta);

                            while let Some(nl) = response[last_line_start..].find('\n') {
                                let line_end = last_line_start + nl;
                                let line = response[last_line_start..line_end].to_string();
                                last_line_start = line_end + 1;

                                let parsed = parse_fence_line(
                                    &line,
                                    &mut in_code_fence,
                                    &mut current_prose,
                                    &mut current_code,
                                    &mut prose_parts,
                                );

                                if !parsed.prose_delta.is_empty() {
                                    emit!(AgentEvent::TextDelta {
                                        content: format!("{}\n", parsed.prose_delta),
                                    });
                                }

                                for code in parsed.codes_to_execute {
                                    code_parts.push(code.clone());
                                    emit!(AgentEvent::CodeBlock { code: code.clone() });
                                    if !acc.had_failure {
                                        execute_and_collect(
                                            &mut self.session,
                                            &code,
                                            &mut acc,
                                            &event_tx,
                                        )
                                        .await;
                                    }
                                    code_executed = true;
                                    if break_on_first_code || !acc.final_response.is_empty() {
                                        break;
                                    }
                                }
                            }
                            if (break_on_first_code && code_executed)
                                || !acc.final_response.is_empty()
                            {
                                break;
                            }
                        }
                    }

                    if (break_on_first_code && code_executed) || !acc.final_response.is_empty() {
                        emit!(AgentEvent::LlmResponse {
                            iteration,
                            content: response.clone(),
                            duration_ms: llm_start.elapsed().as_millis() as u64,
                        });
                        break 'llm_retry response.clone();
                    }

                    emit!(AgentEvent::LlmResponse {
                        iteration,
                        content: if response.is_empty() {
                            llm_response.full_text.clone()
                        } else {
                            response.clone()
                        },
                        duration_ms: llm_start.elapsed().as_millis() as u64,
                    });

                    if last_line_start < response.len() {
                        let trailing = &response[last_line_start..];
                        let parsed = parse_fence_line(
                            trailing,
                            &mut in_code_fence,
                            &mut current_prose,
                            &mut current_code,
                            &mut prose_parts,
                        );
                        if !parsed.prose_delta.is_empty() {
                            emit!(AgentEvent::TextDelta {
                                content: parsed.prose_delta,
                            });
                        }
                        for code in parsed.codes_to_execute {
                            code_parts.push(code.clone());
                            emit!(AgentEvent::CodeBlock { code: code.clone() });
                            if !acc.had_failure {
                                execute_and_collect(&mut self.session, &code, &mut acc, &event_tx)
                                    .await;
                            }
                            code_executed = true;
                            if break_on_first_code || !acc.final_response.is_empty() {
                                break;
                            }
                        }
                    }

                    if in_code_fence && !current_code.trim().is_empty() {
                        code_parts.push(current_code.clone());
                        emit!(AgentEvent::CodeBlock {
                            code: current_code.clone(),
                        });
                        if !acc.had_failure {
                            execute_and_collect(
                                &mut self.session,
                                &current_code,
                                &mut acc,
                                &event_tx,
                            )
                            .await;
                        }
                        current_code.clear();
                        code_executed = true;
                    }

                    let remaining_prose = current_prose.trim().to_string();
                    if !remaining_prose.is_empty() {
                        prose_parts.push(remaining_prose);
                        current_prose.clear();
                    }
                    if response.is_empty() && !llm_response.full_text.is_empty() {
                        response = llm_response.full_text.clone();
                    }

                    if is_malformed_assistant_output(&response) {
                        let preview = truncate_raw_error(response.trim());
                        tracing::warn!(
                            "LLM turn {} produced malformed assistant output on attempt {}/{}: {}",
                            iteration,
                            attempt + 1,
                            LLM_MAX_RETRIES + 1,
                            preview
                        );
                        if attempt < LLM_MAX_RETRIES {
                            last_error = Some(
                                "malformed assistant output from model (partial repl fragment)"
                                    .to_string(),
                            );
                            // Retry with clean per-attempt parsing/execution state.
                            acc = ExecAccumulator::new();
                            response.clear();
                            prose_parts.clear();
                            code_parts.clear();
                            in_code_fence = false;
                            current_prose.clear();
                            current_code.clear();
                            last_line_start = 0;
                            code_executed = false;
                            continue;
                        }

                        emit!(make_error_event(
                            "llm_provider",
                            Some("malformed_output"),
                            "Model returned malformed output. Use /retry to replay this turn.",
                            Some(preview),
                        ));
                        malformed_terminal_failure = true;
                        break 'llm_retry String::new();
                    }

                    break 'llm_retry response.clone();
                }

                // All retries exhausted
                if let Some(err) = last_error {
                    emit!(make_error_event(
                        "llm_provider",
                        Some("retries_exhausted"),
                        format!(
                            "LLM failed after {} attempts: {}. Use /retry to replay the same turn payload.",
                            LLM_MAX_RETRIES + 1,
                            err
                        ),
                        Some(err),
                    ));
                }
                String::new()
            };

            // Clean up message and prompt forwarding
            self.session.clear_message_sender();
            self.session.clear_prompt_sender();
            let _ = drain_handle.await;
            let _ = prompt_drain_handle.await;

            // Collect token usage for all paths that exit the retry block
            // (normal completion, stream error with partial exec, retries exhausted)
            if let Some((usage, request_body, http_summary)) = direct_usage.take() {
                last_input_tokens = usage.input_tokens as usize;
                cumulative_usage.add(&usage);
                emit!(AgentEvent::TokenUsage {
                    iteration,
                    usage: usage.clone(),
                    cumulative: cumulative_usage.clone(),
                });
                log_llm_debug(
                    &self.config.llm_log_path,
                    &self.agent_id,
                    iteration,
                    &usage,
                    request_body,
                    &full_text,
                );
                if let Some(http) = http_summary {
                    tracing::debug!("llm turn {} transport: {}", iteration, http);
                }
            } else {
                last_input_tokens = 0;
            }

            // For mid-stream break: flush any remaining prose
            if code_executed {
                let remaining_prose = current_prose.trim().to_string();
                if !remaining_prose.is_empty() {
                    prose_parts.push(remaining_prose);
                }
            }

            let executed_text = full_text;
            let has_code = !code_parts.is_empty();

            if malformed_terminal_failure {
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }

            // Check for empty response with no execution
            if executed_text.trim().is_empty()
                && acc.tool_calls.is_empty()
                && acc.combined_output.is_empty()
                && !acc.had_failure
            {
                tracing::warn!("LLM returned empty response");
                emit!(make_error_event(
                    "llm_provider",
                    Some("empty_response"),
                    "I didn't get a response. Use /retry to replay this turn, or /provider if credentials changed.",
                    None,
                ));
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }

            // Keep tool images as raw bytes for the next LLM turn and
            // remember their image indices so we can inject multimodal refs.
            let mut next_tool_image_refs: Vec<(usize, String)> = Vec::new();
            let base_image_idx = user_images.len();
            for (i, img) in acc.images.iter().enumerate() {
                tool_images.push((img.mime.clone(), img.data.clone()));
                next_tool_image_refs.push((base_image_idx + i, img.label.clone()));
            }

            // done() = stop signal
            if !acc.final_response.is_empty() {
                emit!(AgentEvent::Message {
                    text: acc.final_response.clone(),
                    kind: "final".to_string(),
                });
                let mid = format!("m{}", msgs.len());
                let asst_parts = build_assistant_parts(&mid, &prose_parts, &code_parts);
                msgs.push(Message {
                    id: mid.clone(),
                    role: MessageRole::Assistant,
                    parts: asst_parts,
                });
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }

            // Inject turn data into _history (skip for mid-stream code breaks)
            if !code_executed {
                let tool_calls_json: Vec<serde_json::Value> = acc
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "tool": tc.tool,
                            "args": tc.args,
                            "result": tc.result,
                            "success": tc.success,
                            "duration_ms": tc.duration_ms,
                        })
                    })
                    .collect();
                // Find the most recent user message for this turn
                let user_msg = msgs
                    .iter()
                    .rev()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| {
                        m.parts
                            .iter()
                            .map(|p| p.content.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let turn_json = serde_json::json!({
                    "index": iteration,
                    "user_message": user_msg,
                    "prose": prose_parts.join("\n\n"),
                    "code": code_parts.join("\n"),
                    "output": acc.combined_output,
                    "error": acc.exec_error,
                    "tool_calls": tool_calls_json,
                });
                let json_str = turn_json
                    .to_string()
                    .replace('\\', "\\\\")
                    .replace('\'', "\\'");
                let mut inject_stmts = Vec::new();
                if history_enabled {
                    inject_stmts.push(format!("_history._add_turn('{json_str}')"));
                }
                if memory_enabled {
                    inject_stmts.push(format!("_mem._set_turn({iteration})"));
                }
                if !inject_stmts.is_empty() {
                    let _ = self.session.run_code(&inject_stmts.join("; ")).await;
                }
            }

            // Build structured feedback parts from accumulated execution state
            {
                let has_output = !acc.combined_output.is_empty();
                let has_tool_calls = !acc.tool_calls.is_empty();

                // Pure prose response with no code execution — turn ends
                if !has_code && !has_output && !has_tool_calls && !acc.had_failure {
                    let mid = format!("m{}", msgs.len());
                    let asst_parts = build_assistant_parts(&mid, &prose_parts, &code_parts);
                    msgs.push(Message {
                        id: mid.clone(),
                        role: MessageRole::Assistant,
                        parts: asst_parts,
                    });
                    if self.config.headless {
                        let sys_id = format!("m{}", msgs.len());
                        let guidance = "Headless mode requires execution via <repl>. \
Prose-only output is not a valid step. Continue with concrete tool execution; call done(...) only from inside <repl> when fully complete.";
                        msgs.push(Message {
                            id: sys_id.clone(),
                            role: MessageRole::System,
                            parts: vec![Part {
                                id: format!("{}.p0", sys_id),
                                kind: PartKind::Error,
                                content: guidance.to_string(),
                                tool_call_id: None,
                                tool_name: None,
                                prune_state: PruneState::Intact,
                            }],
                        });
                        if termination.record_pure_prose_step(self.config.headless) {
                            emit!(make_error_event(
                                "runtime",
                                Some("headless_prose_only"),
                                "Headless run ended after repeated prose-only responses without execution.",
                                None,
                            ));
                            emit!(AgentEvent::Done);
                            return (msgs, iteration);
                        }
                        continue;
                    }
                    emit!(AgentEvent::Done);
                    return (msgs, iteration);
                }
                termination.reset_pure_prose_streak();

                // Build feedback: code blocks wrapped in fences, then output/error
                let mut feedback_parts: Vec<Part> = Vec::new();
                for code in &code_parts {
                    feedback_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Code,
                        content: code.clone(),
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    });
                }

                if has_output {
                    let mut output_text = acc.combined_output;
                    if has_tool_calls {
                        output_text.push_str(&format!(
                            "\n[{} tool call(s) executed]",
                            acc.tool_calls.len()
                        ));
                    }
                    feedback_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Output,
                        content: output_text,
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    });
                } else if has_tool_calls {
                    feedback_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Output,
                        content: format!("[{} tool call(s) executed]", acc.tool_calls.len()),
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    });
                }
                if let Some(err) = &acc.exec_error {
                    feedback_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Error,
                        content: format!("{}\nFix and retry.", err),
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    });
                }
                if !next_tool_image_refs.is_empty() {
                    for (idx, label) in &next_tool_image_refs {
                        feedback_parts.push(Part {
                            id: String::new(),
                            kind: PartKind::Text,
                            content: format!("[Tool image: {}]", label),
                            tool_call_id: None,
                            tool_name: None,
                            prune_state: PruneState::Intact,
                        });
                        feedback_parts.push(Part {
                            id: String::new(),
                            kind: PartKind::Text,
                            content: format!("{IMAGE_REF_PREFIX}{idx}"),
                            tool_call_id: None,
                            tool_name: None,
                            prune_state: PruneState::Intact,
                        });
                    }
                }

                // Push assistant message with prose+code parts
                let asst_id = format!("m{}", msgs.len());
                let asst_parts = build_assistant_parts(&asst_id, &prose_parts, &code_parts);
                msgs.push(Message {
                    id: asst_id.clone(),
                    role: MessageRole::Assistant,
                    parts: asst_parts,
                });

                // Push system feedback with typed parts
                let sys_id = format!("m{}", msgs.len());
                for (idx, part) in feedback_parts.iter_mut().enumerate() {
                    part.id = format!("{}.p{}", sys_id, idx);
                }
                msgs.push(Message {
                    id: sys_id,
                    role: MessageRole::System,
                    parts: feedback_parts,
                });

                // Inject any newly discovered context-aware instructions from file reads.
                let context_text =
                    resolve_context_instructions(instruction_source.as_ref(), &acc.tool_calls);
                self.invalidate_context_cache_if_needed(&acc.tool_calls);
                if !context_text.is_empty() {
                    let instruction_id = format!("m{}", msgs.len());
                    msgs.push(Message {
                        id: instruction_id.clone(),
                        role: MessageRole::System,
                        parts: vec![Part {
                            id: format!("{}.p0", instruction_id),
                            kind: PartKind::Text,
                            content: context_text.clone(),
                            tool_call_id: None,
                            tool_name: None,
                            prune_state: PruneState::Intact,
                        }],
                    });
                }
            }

            iteration += 1;
            // Agent had its grace turn after the turn-limit message — force return
            if termination.should_force_exit_after_grace_turn() {
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }
            termination.maybe_schedule_turn_limit_final(
                iteration,
                run_offset,
                self.config.max_turns,
                &mut msgs,
            );
        }
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

        let ExecutionPreamble {
            model,
            tool_list,
            tool_specs,
            tool_names,
            enabled_capability_ids,
            helper_bindings,
            capability_prompt_sections,
            can_write,
            history_enabled: _,
            memory_enabled: _,
            instruction_source,
            project_instructions,
            base_context,
        } = match self.prepare_execution(ExecutionMode::NativeTools).await {
            Ok(preamble) => preamble,
            Err(event) => {
                emit!(event);
                emit!(AgentEvent::Done);
                return (messages, run_offset);
            }
        };

        let mut cumulative_usage = TokenUsage::default();
        let mut msgs = messages;
        let mut iteration = run_offset;
        let mut attachments: Vec<(String, Vec<u8>)> = images
            .into_iter()
            .map(|png_bytes| ("image/png".to_string(), png_bytes))
            .collect();
        let history_enabled = helper_bindings.contains("search_history")
            || enabled_capability_ids.contains("history");
        let mut last_input_tokens: usize = 0;
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

        struct NativeToolOutcome {
            call_id: String,
            tool_name: String,
            args: serde_json::Value,
            result: crate::ToolResult,
            duration_ms: u64,
        }

        loop {
            if cancel.is_cancelled() {
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }
            let fold = apply_context_folding(
                &mut msgs,
                last_input_tokens,
                max_context,
                self.config.context_folding,
                history_enabled,
            );
            let llm_messages = messages_to_chat(&msgs);
            let context = base_context.clone();
            let include_soul = if self.config.sub_agent {
                self.config.include_soul
            } else {
                true
            };
            let profile = PromptProfile::from_flags(self.config.headless, self.config.sub_agent);
            let system_prompt = compose_system_prompt(PromptComposeInput {
                profile,
                execution_mode: self.config.execution_mode,
                context: &context,
                tool_list: &tool_list,
                tool_names: &tool_names,
                has_history: fold.has_archived_history,
                enabled_capability_ids: &enabled_capability_ids,
                helper_bindings: &helper_bindings,
                capability_prompt_sections: &capability_prompt_sections,
                can_write,
                include_soul,
                project_instructions: &project_instructions,
                overrides: &self.config.prompt_overrides,
            });

            emit!(AgentEvent::LlmRequest {
                iteration,
                message_count: llm_messages.len(),
                tool_list: tool_list.clone(),
            });

            let llm_start = std::time::Instant::now();
            let llm_response = 'llm_retry: {
                let mut last_error = None;
                for attempt in 0..=LLM_MAX_RETRIES {
                    if attempt > 0 {
                        let delay = LLM_RETRY_DELAYS[attempt - 1];
                        emit!(AgentEvent::RetryStatus {
                            wait_seconds: delay.as_secs(),
                            attempt: attempt + 1,
                            max_attempts: LLM_MAX_RETRIES + 1,
                            reason: last_error
                                .clone()
                                .unwrap_or_else(|| "transient provider error".to_string()),
                        });
                        tokio::time::sleep(delay).await;
                    }

                    let llm_request = LlmRequest {
                        model: model.clone(),
                        system_prompt: system_prompt.clone(),
                        messages: llm_messages.clone(),
                        attachments: attachments
                            .iter()
                            .map(|(mime, data)| LlmAttachment {
                                mime: mime.clone(),
                                data: data.clone(),
                            })
                            .collect(),
                        tools: tool_specs.clone(),
                        tool_choice: if tool_specs.is_empty() {
                            LlmToolChoice::None
                        } else {
                            LlmToolChoice::Auto
                        },
                        reasoning_effort: self.config.reasoning_effort.clone(),
                        session_id: self.config.session_id.clone(),
                        stream_events: None,
                    };

                    let mut call_provider = self.config.provider.clone();
                    let llm = adapter_for(&call_provider);
                    match llm.complete(&mut call_provider, llm_request).await {
                        Ok(resp) => {
                            self.config.provider = call_provider;
                            break 'llm_retry resp;
                        }
                        Err(e) => {
                            self.config.provider = call_provider;
                            if e.retryable && attempt < LLM_MAX_RETRIES {
                                last_error = Some(e.message);
                                continue;
                            }
                            emit!(make_error_event(
                                "llm_provider",
                                e.code.as_deref(),
                                format!("LLM error: {}", e.message),
                                e.raw,
                            ));
                            emit!(AgentEvent::Done);
                            return (msgs, iteration);
                        }
                    }
                }

                emit!(make_error_event(
                    "llm_provider",
                    Some("retries_exhausted"),
                    format!("LLM failed after {} attempts.", LLM_MAX_RETRIES + 1),
                    None,
                ));
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            };

            let usage = TokenUsage {
                input_tokens: llm_response.usage.input_tokens,
                output_tokens: llm_response.usage.output_tokens,
                cached_input_tokens: llm_response.usage.cached_input_tokens,
            };
            last_input_tokens = usage.input_tokens as usize;
            cumulative_usage.add(&usage);
            emit!(AgentEvent::TokenUsage {
                iteration,
                usage: usage.clone(),
                cumulative: cumulative_usage.clone(),
            });
            log_llm_debug(
                &self.config.llm_log_path,
                &self.agent_id,
                iteration,
                &usage,
                llm_response.request_body.clone(),
                &llm_response.full_text,
            );

            let response_parts =
                if llm_response.parts.is_empty() && !llm_response.full_text.is_empty() {
                    vec![LlmOutputPart::Text {
                        text: llm_response.full_text.clone(),
                    }]
                } else {
                    llm_response.parts.clone()
                };

            let mut assistant_text = String::new();
            let mut tool_calls: Vec<(String, String, String)> = Vec::new();
            for part in response_parts {
                match part {
                    LlmOutputPart::Text { text } => {
                        if !text.is_empty() {
                            emit!(AgentEvent::TextDelta {
                                content: text.clone(),
                            });
                            assistant_text.push_str(&text);
                        }
                    }
                    LlmOutputPart::ToolCall {
                        call_id,
                        tool_name,
                        input_json,
                    } => {
                        tool_calls.push((call_id, tool_name, input_json));
                    }
                }
            }
            emit!(AgentEvent::LlmResponse {
                iteration,
                content: assistant_text.clone(),
                duration_ms: llm_start.elapsed().as_millis() as u64,
            });

            if tool_calls.is_empty() {
                if assistant_text.trim().is_empty() {
                    emit!(make_error_event(
                        "llm_provider",
                        Some("empty_response"),
                        "Model returned no assistant text or tool calls.",
                        None,
                    ));
                    emit!(AgentEvent::Done);
                    return (msgs, iteration);
                }
                let mid = format!("m{}", msgs.len());
                msgs.push(Message {
                    id: mid.clone(),
                    role: MessageRole::Assistant,
                    parts: vec![Part {
                        id: format!("{}.p0", mid),
                        kind: PartKind::Prose,
                        content: assistant_text.clone(),
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    }],
                });
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }

            let asst_id = format!("m{}", msgs.len());
            let mut assistant_parts = Vec::new();
            if !assistant_text.trim().is_empty() {
                assistant_parts.push(Part {
                    id: format!("{}.p{}", asst_id, assistant_parts.len()),
                    kind: PartKind::Prose,
                    content: assistant_text.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    prune_state: PruneState::Intact,
                });
            }
            let mut pending_tool_parts = Vec::new();
            let mut tool_records = Vec::new();
            let tool_provider = Arc::clone(self.session.tools());
            let mut task_handles = Vec::new();
            for (call_id, tool_name, input_json) in tool_calls {
                pending_tool_parts.push(Part {
                    id: format!(
                        "{}.p{}",
                        asst_id,
                        assistant_parts.len() + pending_tool_parts.len()
                    ),
                    kind: PartKind::ToolCall,
                    content: input_json.clone(),
                    tool_call_id: Some(call_id.clone()),
                    tool_name: Some(tool_name.clone()),
                    prune_state: PruneState::Intact,
                });

                let args = serde_json::from_str::<serde_json::Value>(&input_json)
                    .unwrap_or_else(|_| serde_json::json!({}));
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
                        call_id,
                        tool_name,
                        args,
                        result,
                        duration_ms: tool_start.elapsed().as_millis() as u64,
                    }
                });
                task_handles.push(handle);
            }
            assistant_parts.extend(pending_tool_parts);
            if !assistant_parts.is_empty() {
                msgs.push(Message {
                    id: asst_id,
                    role: MessageRole::Assistant,
                    parts: assistant_parts,
                });
            }

            let mut result_parts = Vec::new();
            for handle in task_handles {
                let mut outcome = match handle.await {
                    Ok(outcome) => outcome,
                    Err(e) => NativeToolOutcome {
                        call_id: uuid::Uuid::new_v4().to_string(),
                        tool_name: "unknown".to_string(),
                        args: serde_json::json!({}),
                        result: crate::ToolResult::err_fmt(format!("tool task panicked: {e}")),
                        duration_ms: 0,
                    },
                };

                emit!(AgentEvent::ToolCall {
                    name: outcome.tool_name.clone(),
                    args: outcome.args.clone(),
                    result: outcome.result.result.clone(),
                    success: outcome.result.success,
                    duration_ms: outcome.duration_ms,
                });
                tool_records.push(crate::ToolCallRecord {
                    tool: outcome.tool_name.clone(),
                    args: outcome.args.clone(),
                    result: outcome.result.result.clone(),
                    success: outcome.result.success,
                    duration_ms: outcome.duration_ms,
                });

                result_parts.push(Part {
                    id: String::new(),
                    kind: PartKind::ToolResult,
                    content: format_tool_result_content(
                        outcome.result.success,
                        &outcome.result.result,
                    ),
                    tool_call_id: Some(outcome.call_id.clone()),
                    tool_name: Some(outcome.tool_name.clone()),
                    prune_state: PruneState::Intact,
                });

                let base_image_idx = attachments.len();
                for (image_offset, image) in std::mem::take(&mut outcome.result.images)
                    .into_iter()
                    .enumerate()
                {
                    attachments.push((image.mime.clone(), image.data.clone()));
                    let image_idx = base_image_idx + image_offset;
                    result_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Text,
                        content: format!("[Tool image: {}]", image.label),
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    });
                    result_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Text,
                        content: format!("{IMAGE_REF_PREFIX}{image_idx}"),
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    });
                }
            }

            if !result_parts.is_empty() {
                let user_id = format!("m{}", msgs.len());
                for (idx, part) in result_parts.iter_mut().enumerate() {
                    part.id = format!("{}.p{}", user_id, idx);
                }
                msgs.push(Message {
                    id: user_id,
                    role: MessageRole::User,
                    parts: result_parts,
                });
                let context_text =
                    resolve_context_instructions(instruction_source.as_ref(), &tool_records);
                self.invalidate_context_cache_if_needed(&tool_records);
                if !context_text.is_empty() {
                    let instruction_id = format!("m{}", msgs.len());
                    msgs.push(Message {
                        id: instruction_id.clone(),
                        role: MessageRole::System,
                        parts: vec![Part {
                            id: format!("{}.p0", instruction_id),
                            kind: PartKind::Text,
                            content: context_text.clone(),
                            tool_call_id: None,
                            tool_name: None,
                            prune_state: PruneState::Intact,
                        }],
                    });
                }
            }

            iteration += 1;
            if let Some(max_turns) = self.config.max_turns
                && iteration >= run_offset + max_turns
            {
                let sys_id = format!("m{}", msgs.len());
                msgs.push(Message {
                    id: sys_id.clone(),
                    role: MessageRole::System,
                    parts: vec![Part {
                        id: format!("{}.p0", sys_id),
                        kind: PartKind::Error,
                        content: format!(
                            "Turn limit reached ({max_turns}) before a final assistant response."
                        ),
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    }],
                });
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }
        }
    }
}

/// Log raw LLM request/response to a debug file (if configured).
fn log_llm_debug(
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

fn format_tool_result_content(success: bool, result: &serde_json::Value) -> String {
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

/// Discover importable third-party packages available to the embedded REPL.
/// Returns None if discovery fails.
fn repl_third_party_packages() -> Option<Vec<String>> {
    let lib_dir = crate::python_home::ensure_python_home().ok()?;
    let site_packages = lib_dir.join("python3.14").join("site-packages");
    if !site_packages.exists() {
        return Some(Vec::new());
    }

    let ignored = [
        "dill",
        "pip",
        "setuptools",
        "wheel",
        "pkg-resources",
        "pkg_resources",
    ];

    let mut names: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&site_packages).ok()?;
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.ends_with(".dist-info") {
            continue;
        }
        let Some(name) = parse_dist_info_package_name(&entry.path(), &file_name) else {
            continue;
        };
        let normalized = name.trim().to_ascii_lowercase();
        if normalized.is_empty() || ignored.contains(&normalized.as_str()) {
            continue;
        }
        names.push(normalized);
    }

    names.sort();
    names.dedup();
    Some(names)
}

fn parse_dist_info_package_name(dist_info_dir: &std::path::Path, dir_name: &str) -> Option<String> {
    // Prefer canonical name from METADATA
    let metadata = dist_info_dir.join("METADATA");
    if let Ok(text) = std::fs::read_to_string(&metadata)
        && let Some(line) = text.lines().find(|line| line.starts_with("Name:"))
    {
        let name = line.trim_start_matches("Name:").trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    // Fallback: parse "<name>-<version>.dist-info"
    let stem = dir_name.strip_suffix(".dist-info")?;
    let split_idx = stem
        .char_indices()
        .find_map(|(i, c)| {
            if c == '-' {
                stem.get(i + 1..i + 2)
                    .and_then(|s| s.chars().next())
                    .filter(|n| n.is_ascii_digit())
                    .map(|_| i)
            } else {
                None
            }
        })
        .unwrap_or(stem.len());
    Some(stem[..split_idx].to_string())
}

/// Build alternating Prose/Code parts for an assistant message.
fn build_assistant_parts(msg_id: &str, prose_parts: &[String], code_parts: &[String]) -> Vec<Part> {
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

struct FenceLineParse {
    prose_delta: String,
    codes_to_execute: Vec<String>,
}

fn append_line_segment(target: &mut String, segment: &str, line_started: &mut bool) {
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
fn parse_fence_line(
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
fn resolve_context_instructions(
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
