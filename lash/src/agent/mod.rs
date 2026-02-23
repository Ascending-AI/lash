mod exec;
pub mod message;
mod prompt;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ToolDefinition;
use crate::instructions::{FsInstructionSource, InstructionSource};
use crate::llm::factory::adapter_for;
use crate::llm::types::{LlmAttachment, LlmRequest, LlmResponse, LlmStreamEvent};
use crate::provider::Provider;
use crate::session::Session;

pub use message::{Message, MessageRole, Part, PartKind, PruneState, messages_to_chat};

use exec::{ExecAccumulator, execute_and_collect};
use message::IMAGE_REF_PREFIX;
use prompt::{PromptComposeInput, PromptProfile, compose_system_prompt};
pub use prompt::{PromptOverrideMode, PromptSectionName, PromptSectionOverride};

/// Send an event to the channel if it's still open.
pub(crate) async fn send_event(tx: &mpsc::Sender<AgentEvent>, event: AgentEvent) {
    if !tx.is_closed() {
        let _ = tx.send(event).await;
    }
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
    /// LLM provider (OpenRouter, Claude OAuth, Codex, or Google OAuth)
    pub provider: Provider,
    /// Override for context window size (tokens). If None, looked up from model_info.
    pub max_context_tokens: Option<usize>,
    /// When true, use SubAgentStep prompt instead of CodeActStep
    pub sub_agent: bool,
    /// Optional reasoning effort level (e.g. "medium", "high") for Codex
    pub reasoning_effort: Option<String>,
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
}

/// Backend capability gates for REPL globals and prompt sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub memory: bool,
    pub history: bool,
}

impl Default for AgentCapabilities {
    fn default() -> Self {
        Self {
            memory: true,
            history: true,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            capabilities: AgentCapabilities::default(),
            model: "anthropic/claude-sonnet-4.6".to_string(),
            provider: Provider::OpenRouter {
                api_key: String::new(),
                base_url: "https://openrouter.ai/api/v1".to_string(),
            },
            max_context_tokens: None,
            sub_agent: false,
            reasoning_effort: None,
            max_turns: None,
            include_soul: false,
            llm_log_path: None,
            headless: false,
            prompt_overrides: Vec::new(),
            instruction_source: Arc::new(FsInstructionSource::new()),
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

/// Events emitted during an agent run.
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

/// CodeAct agent: LLM writes Python code, REPL executes, output feeds back.
pub struct Agent {
    agent_id: String,
    session: Session,
    config: AgentConfig,
}

impl Agent {
    pub fn new(session: Session, config: AgentConfig, agent_id: Option<String>) -> Self {
        let agent_id = agent_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Self {
            agent_id,
            session,
            config,
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

    pub fn capabilities(&self) -> AgentCapabilities {
        self.config.capabilities
    }

    /// Reset the underlying Python session (clear namespace).
    pub async fn reset_session(&mut self) -> Result<(), crate::SessionError> {
        self.session.reset().await
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

        // Refresh tokens if needed before starting
        match self.config.provider.ensure_fresh().await {
            Ok(true) => {
                let _ = crate::provider::save_provider(&self.config.provider);
            }
            Err(e) => {
                emit!(make_error_event(
                    "token_refresh",
                    Some("refresh_failed"),
                    format!(
                        "Token refresh failed: {}. Re-authenticate with /provider and retry.",
                        e
                    ),
                    Some(e.to_string()),
                ));
                emit!(AgentEvent::Done);
                return (messages, run_offset);
            }
            _ => {}
        }

        let llm = adapter_for(&self.config.provider);
        let model = llm.normalize_model(&self.config.model);

        // Generate tool docs, context, and dynamic prompt sections
        let all_tools = self.session.tools().definitions();
        let visible: Vec<_> = all_tools.iter().filter(|t| !t.hidden).cloned().collect();
        let prompt_tools: Vec<_> = visible
            .iter()
            .filter(|t| t.inject_into_prompt)
            .cloned()
            .collect();
        let mut tool_list = ToolDefinition::format_tool_docs(&prompt_tools);
        let omitted_tool_count = visible.iter().filter(|t| !t.inject_into_prompt).count();
        if omitted_tool_count > 0 {
            let note = format!(
                "\n\n- **Note:** {omitted_tool_count} additional tool(s) are available but omitted from this prompt for brevity. Use `list_tools()` / `search_tools(...)` to discover them, then call via `tools.<tool>(...)`."
            );
            tool_list.push_str(&note);
        }
        let base_context = build_context();
        let tool_names: Vec<String> = visible.iter().map(|t| t.name.clone()).collect();
        let instruction_source = Arc::clone(&self.config.instruction_source);
        let project_instructions = instruction_source.system_instructions();

        // Keep raw image bytes so provider-specific transports can materialize
        // multimodal payloads in their native wire format.
        let user_images: Vec<(String, Vec<u8>)> = images
            .into_iter()
            .map(|png_bytes| ("image/png".to_string(), png_bytes))
            .collect();

        // Initialize provider-specific runtime state (e.g., Cloud Code project resolution).
        match llm.ensure_ready(&mut self.config.provider).await {
            Ok(changed) => {
                if changed {
                    let _ = crate::provider::save_provider(&self.config.provider);
                }
            }
            Err(e) => {
                emit!(make_error_event(
                    "llm_provider",
                    e.code.as_deref(),
                    format!(
                        "LLM provider initialization failed: {}. Run /provider to reconfigure credentials, then retry.",
                        e.message
                    ),
                    e.raw,
                ));
                emit!(AgentEvent::Done);
                return (messages, run_offset);
            }
        }

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
        let mut max_steps_final = false;
        let mut has_history = false;
        let mut context_pruned_turns: usize = 0;
        let session_start = std::time::Instant::now();
        let mut headless_prose_only_streak: usize = 0;
        let history_enabled = self.config.capabilities.history;
        let memory_enabled = self.config.capabilities.memory;

        loop {
            if cancel.is_cancelled() {
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }

            // Rolling window: when context exceeds 60% of the model's token limit,
            // collapse old turns to make room. Always preserve all User messages.
            // Uses actual input_tokens from the last API call for accurate measurement,
            // with char-based estimation (~4 chars/token) as proportional weights.
            msgs.retain(|m| m.id != "history_note");
            {
                let token_budget = max_context * 60 / 100;
                let needs_pruning = last_input_tokens > token_budget;

                // `keep_from` is the start index of the retained tail.
                // Default to 1 (= keep everything after the initial system message).
                // If we are not pruning this turn, [1..keep_from) is empty and no collapse happens.
                let mut keep_from = 1usize;
                if needs_pruning {
                    keep_from = msgs.len();
                    // Estimate how many chars to keep: scale by (budget / actual) ratio
                    let total_chars: usize = msgs.iter().map(|m| m.char_count()).sum();
                    let target_chars = total_chars * token_budget / last_input_tokens.max(1);

                    // Walk backwards, accumulating char counts as proportional weights
                    let mut tail_chars = 0usize;
                    for i in (1..msgs.len()).rev() {
                        let cost = msgs[i].char_count();
                        if tail_chars + cost > target_chars {
                            break;
                        }
                        tail_chars += cost;
                        keep_from = i;
                    }
                }
                // Collect User messages from the pruned region [1..keep_from)
                // so they're preserved in the context.
                let mut preserved_user_msgs: Vec<Message> = Vec::new();
                if keep_from > 1 {
                    for msg in &msgs[1..keep_from] {
                        if msg.role == MessageRole::User {
                            preserved_user_msgs.push(msg.clone());
                        }
                    }
                }
                // Count collapsed turns (assistant+system pairs, minus preserved users)
                let collapsed_count = if keep_from > 1 {
                    (keep_from - 1 - preserved_user_msgs.len()) / 2
                } else {
                    0
                };
                if collapsed_count > 0 {
                    context_pruned_turns += collapsed_count;
                    has_history = true;
                    // Drain the pruned region
                    msgs.drain(1..keep_from);
                    // Re-insert preserved user messages, then the history note
                    let elapsed = session_start.elapsed();
                    let elapsed_str = if elapsed.as_secs() >= 3600 {
                        format!(
                            "{}h {}m",
                            elapsed.as_secs() / 3600,
                            (elapsed.as_secs() % 3600) / 60
                        )
                    } else if elapsed.as_secs() >= 60 {
                        format!("{}m {}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
                    } else {
                        format!("{}s", elapsed.as_secs())
                    };
                    let note = if history_enabled {
                        format!(
                            "[Turn {}, {} elapsed. {} earlier turn(s) dropped from context (not summarized). \
                             Use `_history` to access them at full fidelity:\n \
                             `_history.user_messages()` -- what the user asked\n \
                             `_history.search(\"query\", mode=\"hybrid\")` -- search past results\n \
                             `_history[i]` -- specific turn]",
                            iteration, elapsed_str, collapsed_count
                        )
                    } else {
                        format!(
                            "[Turn {}, {} elapsed. {} earlier turn(s) dropped from context (not summarized).]",
                            iteration, elapsed_str, collapsed_count
                        )
                    };
                    msgs.insert(
                        1,
                        Message {
                            id: "history_note".to_string(),
                            role: MessageRole::System,
                            parts: vec![Part {
                                id: "history_note.p0".to_string(),
                                kind: PartKind::Text,
                                content: note,
                                prune_state: PruneState::Intact,
                            }],
                        },
                    );
                    // Insert preserved user messages right after the history note
                    for (idx, user_msg) in preserved_user_msgs.into_iter().enumerate() {
                        msgs.insert(2 + idx, user_msg);
                    }
                }
            }

            let history_scope = if self.config.sub_agent {
                if history_enabled {
                    format!(
                        "Context-pruned turns this run: {}. This count tracks pruning in this agent only; inherited parent history may also exist in `_history`.",
                        context_pruned_turns
                    )
                } else {
                    format!("Context-pruned turns this run: {}.", context_pruned_turns)
                }
            } else {
                if history_enabled {
                    format!(
                        "Context-pruned turns this run: {}. If this is 0, avoid `_history` mining detours unless prior-turn context is explicitly needed.",
                        context_pruned_turns
                    )
                } else {
                    format!("Context-pruned turns this run: {}.", context_pruned_turns)
                }
            };
            let context = if base_context.is_empty() {
                history_scope
            } else {
                format!("{base_context}\n{history_scope}")
            };

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
                        context: &context,
                        tool_list: &tool_list,
                        tool_names: &tool_names,
                        has_history,
                        history_enabled,
                        memory_enabled,
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
                        reasoning_effort: self.config.reasoning_effort.clone(),
                        stream_events: None,
                    };

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

                    let break_on_first_code = !self.is_root_interactive();
                    let mut streamed_delta_count = 0usize;
                    let mut stop_stream_processing = false;
                    let mut retry_after_error = false;
                    let mut llm_task = llm_task;
                    let llm_response: LlmResponse = loop {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                llm_task.abort();
                                self.session.clear_message_sender();
                                self.session.clear_prompt_sender();
                                emit!(AgentEvent::Done);
                                return (msgs, iteration);
                            }
                            maybe_event = llm_stream_rx.recv() => {
                                let Some(event) = maybe_event else { continue };
                                match event {
                                    LlmStreamEvent::Delta(delta) => {
                                        if stop_stream_processing || delta.is_empty() {
                                            continue;
                                        }
                                        streamed_delta_count += 1;
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
                                    LlmStreamEvent::Usage(_) => {}
                                }
                            }
                            join = &mut llm_task => {
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
                        headless_prose_only_streak += 1;
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
                                prune_state: PruneState::Intact,
                            }],
                        });
                        if headless_prose_only_streak >= 3 {
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
                headless_prose_only_streak = 0;

                // Build feedback: code blocks wrapped in fences, then output/error
                let mut feedback_parts: Vec<Part> = Vec::new();
                for code in &code_parts {
                    feedback_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Code,
                        content: code.clone(),
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
                        prune_state: PruneState::Intact,
                    });
                } else if has_tool_calls {
                    feedback_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Output,
                        content: format!("[{} tool call(s) executed]", acc.tool_calls.len()),
                        prune_state: PruneState::Intact,
                    });
                }
                if let Some(err) = &acc.exec_error {
                    feedback_parts.push(Part {
                        id: String::new(),
                        kind: PartKind::Error,
                        content: format!("{}\nFix and retry.", err),
                        prune_state: PruneState::Intact,
                    });
                }
                if !next_tool_image_refs.is_empty() {
                    for (idx, label) in &next_tool_image_refs {
                        feedback_parts.push(Part {
                            id: String::new(),
                            kind: PartKind::Text,
                            content: format!("[Tool image: {}]", label),
                            prune_state: PruneState::Intact,
                        });
                        feedback_parts.push(Part {
                            id: String::new(),
                            kind: PartKind::Text,
                            content: format!("{IMAGE_REF_PREFIX}{idx}"),
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
                if !context_text.is_empty() {
                    let instruction_id = format!("m{}", msgs.len());
                    msgs.push(Message {
                        id: instruction_id.clone(),
                        role: MessageRole::System,
                        parts: vec![Part {
                            id: format!("{}.p0", instruction_id),
                            kind: PartKind::Text,
                            content: context_text,
                            prune_state: PruneState::Intact,
                        }],
                    });
                }
            }

            iteration += 1;
            // Agent had its grace turn after the turn-limit message — force return
            if max_steps_final {
                emit!(AgentEvent::Done);
                return (msgs, iteration);
            }
            if let Some(max) = self.config.max_turns
                && iteration >= run_offset + max
            {
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
                        prune_state: PruneState::Intact,
                    }],
                });
                max_steps_final = true;
            }
        }
    }
}

impl Agent {
    fn is_root_interactive(&self) -> bool {
        !self.config.headless && !self.config.sub_agent
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
    use super::parse_fence_line;

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
