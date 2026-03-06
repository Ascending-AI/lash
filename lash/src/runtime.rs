use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::message::IMAGE_REF_PREFIX;
use crate::agent::{
    Agent, AgentConfig, AgentEvent, Message, MessageRole, Part, PartKind, PromptSectionOverride,
    PruneState, TokenUsage,
};
use crate::capabilities::AgentCapabilities;
use crate::instructions::{FsInstructionSource, InstructionSource};
use crate::provider::Provider;
use crate::strip_repl_fragments;
use crate::{
    CapabilityId, ContextFoldingConfig, ExecutionMode, Session, SessionError, ToolCallRecord,
    ToolProvider,
};

/// Runtime execution mode for a turn.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum RunMode {
    Normal,
    Plan,
}

/// Host-provided per-turn input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Text { text: String },
    FileRef { path: String },
    DirRef { path: String },
    ImageRef { id: String },
    SkillRef { name: String, args: Option<String> },
}

/// Host-provided per-turn input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnInput {
    pub items: Vec<InputItem>,
    #[serde(default)]
    pub image_blobs: HashMap<String, Vec<u8>>,
    #[serde(default)]
    pub mode: Option<RunMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<String>,
}

#[derive(Clone, Debug)]
enum NormalizedItem {
    Text(String),
    Image(Vec<u8>),
}

/// Serializable host-owned session envelope.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentStateEnvelope {
    pub agent_id: String,
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub context_folding: ContextFoldingConfig,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub iteration: usize,
    #[serde(default)]
    pub token_usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_state: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_state: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_manifest: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repl_snapshot: Option<Vec<u8>>,
}

impl Default for AgentStateEnvelope {
    fn default() -> Self {
        Self {
            agent_id: "root".to_string(),
            execution_mode: ExecutionMode::Repl,
            context_folding: ContextFoldingConfig::default(),
            messages: Vec::new(),
            iteration: 0,
            token_usage: TokenUsage::default(),
            task_state: None,
            subagent_state: None,
            replay_manifest: None,
            repl_snapshot: None,
        }
    }
}

/// Canonical assistant output payload.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AssistantOutput {
    pub safe_text: String,
    pub raw_text: String,
    pub state: OutputState,
}

/// Quality and usability of assembled terminal output.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputState {
    Usable,
    EmptyOutput,
    TracebackOnly,
    Sanitized,
    RecoveredFromError,
}

/// Structured terminal status for a turn.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

/// Canonical reason a turn ended.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoneReason {
    ModelStop,
    MaxTurns,
    UserAbort,
    ToolFailure,
    RuntimeError,
}

/// REPL code execution output observed during a turn.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeOutputRecord {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// High-level execution summary shared across execution modes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExecutionSummary {
    pub mode: ExecutionMode,
    #[serde(default)]
    pub had_tool_calls: bool,
    #[serde(default)]
    pub had_code_execution: bool,
}

/// Structured issue surfaced during turn execution.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnIssue {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
}

/// Canonical high-level turn result returned to hosts.
/// This contract is stable across execution modes; mode-specific detail is summarized in
/// `execution`, while REPL-only detail is exposed through `code_outputs`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AssembledTurn {
    pub state: AgentStateEnvelope,
    pub status: TurnStatus,
    pub assistant_output: AssistantOutput,
    pub done_reason: DoneReason,
    pub execution: ExecutionSummary,
    #[serde(default)]
    pub token_usage: TokenUsage,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default)]
    pub code_outputs: Vec<CodeOutputRecord>,
    #[serde(default)]
    pub errors: Vec<TurnIssue>,
}

/// Runtime error for unexpected failures.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// Host profile presets for runtime policies.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HostProfile {
    #[default]
    Interactive,
    Headless,
    Embedded,
}

/// Pluggable path resolver for file and directory references.
pub trait PathResolver: Send + Sync {
    fn resolve(&self, path: &str, expect_file: bool, base_dir: &Path) -> Result<PathBuf, String>;
}

/// Sanitization policy knobs.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SanitizerPolicy {
    #[serde(default = "SanitizerPolicy::default_strip_repl_fragments")]
    pub strip_repl_fragments: bool,
}

impl SanitizerPolicy {
    fn default_strip_repl_fragments() -> bool {
        true
    }
}

impl Default for SanitizerPolicy {
    fn default() -> Self {
        Self {
            strip_repl_fragments: true,
        }
    }
}

/// Termination policy knobs.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TerminationPolicy {
    #[serde(default)]
    pub treat_missing_done_as_failure: bool,
}

impl Default for TerminationPolicy {
    fn default() -> Self {
        Self {
            treat_missing_done_as_failure: true,
        }
    }
}

/// Host event sink for low-level streaming runtime events.
/// `AgentEvent` is intentionally mode-specific and should be treated as preview/progress data.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: AgentEvent);
}

/// No-op sink useful for callers that only care about final state.
pub struct NoopEventSink;

#[async_trait::async_trait]
impl EventSink for NoopEventSink {
    async fn emit(&self, _event: AgentEvent) {}
}

#[derive(Default)]
struct TurnAssembler {
    final_message: Option<String>,
    text_deltas: String,
    tool_calls: Vec<ToolCallRecord>,
    code_outputs: Vec<CodeOutputRecord>,
    token_usage: TokenUsage,
    issues: Vec<TurnIssue>,
    saw_done: bool,
    saw_tool_failure: bool,
}

impl TurnAssembler {
    fn push(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TextDelta { content } => {
                self.text_deltas.push_str(content);
            }
            AgentEvent::ToolCall {
                name,
                args,
                result,
                success,
                duration_ms,
            } => {
                self.tool_calls.push(ToolCallRecord {
                    tool: name.clone(),
                    args: args.clone(),
                    result: result.clone(),
                    success: *success,
                    duration_ms: *duration_ms,
                });
                if !success {
                    self.saw_tool_failure = true;
                }
            }
            AgentEvent::CodeOutput { output, error } => {
                if error.is_some() {
                    self.saw_tool_failure = true;
                }
                self.code_outputs.push(CodeOutputRecord {
                    output: output.clone(),
                    error: error.clone(),
                });
            }
            AgentEvent::Message { text, kind } if kind == "final" => {
                self.final_message = Some(text.clone());
            }
            AgentEvent::TokenUsage { cumulative, .. } => {
                self.token_usage = cumulative.clone();
            }
            AgentEvent::Error { message, envelope } => {
                let (kind, code) = if let Some(envelope) = envelope {
                    (envelope.kind.clone(), envelope.code.clone())
                } else {
                    ("runtime".to_string(), None)
                };
                self.issues.push(TurnIssue {
                    kind,
                    code,
                    message: message.clone(),
                });
            }
            AgentEvent::Done => {
                self.saw_done = true;
            }
            _ => {}
        }
    }

    fn finish(
        self,
        state: AgentStateEnvelope,
        interrupted: bool,
        force_runtime_error: Option<TurnIssue>,
        sanitizer: &SanitizerPolicy,
        termination: &TerminationPolicy,
    ) -> AssembledTurn {
        let mut issues = self.issues;
        if let Some(issue) = force_runtime_error {
            issues.push(issue);
        }
        let max_turn_reached = state.messages.iter().rev().take(8).any(|msg| {
            msg.role == MessageRole::System
                && msg
                    .parts
                    .iter()
                    .any(|part| part.content.contains("Turn limit reached ("))
        });

        let raw_output = if let Some(final_message) = self.final_message {
            final_message
        } else {
            self.text_deltas.trim().to_string()
        };
        let safe_output = sanitize_assistant_output(raw_output.clone(), sanitizer);
        let output_state = classify_output_state(&raw_output, &safe_output, &issues);

        let (status, done_reason) = if interrupted {
            (TurnStatus::Interrupted, DoneReason::UserAbort)
        } else if !self.saw_done && termination.treat_missing_done_as_failure {
            (TurnStatus::Failed, DoneReason::RuntimeError)
        } else if !issues.is_empty() {
            if self.saw_tool_failure {
                (TurnStatus::Failed, DoneReason::ToolFailure)
            } else {
                (TurnStatus::Failed, DoneReason::RuntimeError)
            }
        } else if max_turn_reached {
            (TurnStatus::Completed, DoneReason::MaxTurns)
        } else {
            (TurnStatus::Completed, DoneReason::ModelStop)
        };

        AssembledTurn {
            execution: ExecutionSummary {
                mode: state.execution_mode,
                had_tool_calls: !self.tool_calls.is_empty(),
                had_code_execution: !self.code_outputs.is_empty(),
            },
            state,
            status,
            assistant_output: AssistantOutput {
                safe_text: safe_output,
                raw_text: raw_output,
                state: output_state,
            },
            done_reason,
            token_usage: self.token_usage,
            tool_calls: self.tool_calls,
            code_outputs: self.code_outputs,
            errors: issues,
        }
    }
}

/// Runtime config used by embedders to construct an engine.
#[derive(Clone)]
pub struct RuntimeConfig {
    pub capabilities: AgentCapabilities,
    pub model: String,
    pub provider: Provider,
    pub execution_mode: ExecutionMode,
    pub context_folding: ContextFoldingConfig,
    pub session_id: Option<String>,
    pub max_context_tokens: Option<usize>,
    pub include_soul: bool,
    pub llm_log_path: Option<PathBuf>,
    pub headless: bool,
    pub host_profile: HostProfile,
    pub prompt_overrides: Vec<PromptSectionOverride>,
    pub base_dir: Option<PathBuf>,
    pub path_resolver: Option<Arc<dyn PathResolver>>,
    pub sanitizer: SanitizerPolicy,
    pub termination: TerminationPolicy,
    pub instruction_source: Arc<dyn InstructionSource>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let cfg = AgentConfig::default();
        Self {
            capabilities: cfg.capabilities,
            model: cfg.model,
            provider: cfg.provider,
            execution_mode: cfg.execution_mode,
            context_folding: cfg.context_folding,
            session_id: cfg.session_id,
            max_context_tokens: cfg.max_context_tokens,
            include_soul: cfg.include_soul,
            llm_log_path: cfg.llm_log_path,
            headless: cfg.headless,
            host_profile: if cfg.headless {
                HostProfile::Headless
            } else {
                HostProfile::Interactive
            },
            prompt_overrides: cfg.prompt_overrides,
            base_dir: None,
            path_resolver: None,
            sanitizer: SanitizerPolicy::default(),
            termination: TerminationPolicy::default(),
            instruction_source: Arc::new(FsInstructionSource::new()),
        }
    }
}

impl From<RuntimeConfig> for AgentConfig {
    fn from(value: RuntimeConfig) -> Self {
        let headless = value.headless || matches!(value.host_profile, HostProfile::Headless);
        Self {
            capabilities: value.capabilities,
            model: value.model,
            provider: value.provider,
            execution_mode: value.execution_mode,
            context_folding: value.context_folding,
            session_id: value.session_id,
            max_context_tokens: value.max_context_tokens,
            sub_agent: false,
            reasoning_effort: None,
            max_turns: None,
            include_soul: value.include_soul,
            llm_log_path: value.llm_log_path,
            headless,
            prompt_overrides: value.prompt_overrides,
            instruction_source: value.instruction_source,
        }
    }
}

impl From<AgentConfig> for RuntimeConfig {
    fn from(value: AgentConfig) -> Self {
        let host_profile = if value.headless {
            HostProfile::Headless
        } else {
            HostProfile::Interactive
        };
        Self {
            capabilities: value.capabilities,
            model: value.model,
            provider: value.provider,
            execution_mode: value.execution_mode,
            context_folding: value.context_folding,
            session_id: value.session_id,
            max_context_tokens: value.max_context_tokens,
            include_soul: value.include_soul,
            llm_log_path: value.llm_log_path,
            headless: value.headless,
            host_profile,
            prompt_overrides: value.prompt_overrides,
            base_dir: None,
            path_resolver: None,
            sanitizer: SanitizerPolicy::default(),
            termination: TerminationPolicy::default(),
            instruction_source: value.instruction_source,
        }
    }
}

/// Generic runtime engine for CLI or programmatic embedding.
pub struct RuntimeEngine {
    agent: Option<Agent>,
    state: AgentStateEnvelope,
    capabilities: AgentCapabilities,
    host_profile: HostProfile,
    base_dir: PathBuf,
    path_resolver: Option<Arc<dyn PathResolver>>,
    sanitizer: SanitizerPolicy,
    termination: TerminationPolicy,
}

impl RuntimeEngine {
    /// Build a runtime from host-provided state + tools.
    pub async fn from_state(
        config: RuntimeConfig,
        tools: Arc<dyn ToolProvider>,
        mut state: AgentStateEnvelope,
    ) -> Result<Self, SessionError> {
        let capabilities = config.capabilities.clone();
        let host_profile = config.host_profile.clone();
        let base_dir = config
            .base_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let path_resolver = config.path_resolver.clone();
        let sanitizer = config.sanitizer.clone();
        let termination = config.termination.clone();
        if state.agent_id.is_empty() {
            state.agent_id = "root".to_string();
        }
        if matches!(state.execution_mode, ExecutionMode::Repl)
            && !matches!(config.execution_mode, ExecutionMode::Repl)
        {
            state.execution_mode = config.execution_mode;
        }
        if state.context_folding.is_default() && !config.context_folding.is_default() {
            state.context_folding = config.context_folding;
        }
        tools.set_execution_mode(state.execution_mode);
        let session = Session::new(
            tools,
            &state.agent_id,
            config.headless,
            capabilities.clone(),
        )
        .await?;
        let mut agent = Agent::new(session, config.into(), Some(state.agent_id.clone()));
        agent.set_execution_mode(state.execution_mode);
        agent.set_context_folding(state.context_folding);
        if let Some(snapshot) = state.repl_snapshot.clone() {
            agent.restore(&snapshot).await?;
        }
        Ok(Self {
            agent: Some(agent),
            state,
            capabilities,
            host_profile,
            base_dir,
            path_resolver,
            sanitizer,
            termination,
        })
    }

    /// Export current host-owned state envelope.
    pub fn export_state(&self) -> AgentStateEnvelope {
        self.state.clone()
    }

    /// Replace the host-owned state envelope.
    pub fn set_state(&mut self, state: AgentStateEnvelope) {
        if let Some(agent) = self.agent.as_mut() {
            agent.set_execution_mode(state.execution_mode);
            agent.set_context_folding(state.context_folding);
        }
        self.state = state;
    }

    /// Update model on the underlying agent.
    pub fn set_model(&mut self, model: String) {
        if let Some(agent) = self.agent.as_mut() {
            agent.set_model(model);
        }
    }

    /// Update reasoning effort on the underlying agent.
    pub fn set_reasoning_effort(&mut self, reasoning_effort: Option<String>) {
        if let Some(agent) = self.agent.as_mut() {
            agent.set_reasoning_effort(reasoning_effort);
        }
    }

    /// Update provider on the underlying agent.
    pub fn set_provider(&mut self, provider: Provider) {
        if let Some(agent) = self.agent.as_mut() {
            agent.set_provider(provider);
        }
    }

    /// Update session ID metadata on the underlying agent.
    pub fn set_session_id(&mut self, session_id: Option<String>) {
        if let Some(agent) = self.agent.as_mut() {
            agent.set_session_id(session_id);
        }
    }

    /// Update execution mode on the underlying agent and persisted envelope.
    pub fn set_execution_mode(&mut self, execution_mode: ExecutionMode) {
        self.state.execution_mode = execution_mode;
        if let Some(agent) = self.agent.as_mut() {
            agent.set_execution_mode(execution_mode);
        }
    }

    /// Update context folding policy on the underlying agent and persisted envelope.
    pub fn set_context_folding(&mut self, context_folding: ContextFoldingConfig) {
        self.state.context_folding = context_folding;
        if let Some(agent) = self.agent.as_mut() {
            agent.set_context_folding(context_folding);
        }
    }

    /// Update the active prompt-facing capability set.
    pub fn set_capabilities(&mut self, capabilities: AgentCapabilities) {
        self.capabilities = capabilities.clone();
        if let Some(agent) = self.agent.as_mut() {
            agent.set_capabilities(capabilities);
        }
    }

    /// Re-register the current tool/capability projection in the live Python session.
    pub async fn reconfigure_session(
        &mut self,
        capabilities_json: String,
        generation: u64,
    ) -> Result<(), SessionError> {
        let Some(agent) = self.agent.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime agent not available".to_string(),
            ));
        };
        agent
            .reconfigure_session(capabilities_json, generation)
            .await
    }

    /// Reset the REPL session on the underlying agent.
    pub async fn reset_session(&mut self) -> Result<(), SessionError> {
        let Some(agent) = self.agent.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime agent not available".to_string(),
            ));
        };
        agent.reset_session().await
    }

    /// Explicitly snapshot REPL state; does not run an LLM turn.
    pub async fn snapshot_repl(&mut self) -> Result<Vec<u8>, SessionError> {
        let Some(agent) = self.agent.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime agent not available".to_string(),
            ));
        };
        let Some(blob) = agent.snapshot().await else {
            return Err(SessionError::Protocol(
                "failed to snapshot runtime repl".to_string(),
            ));
        };
        self.state.repl_snapshot = Some(blob.clone());
        Ok(blob)
    }

    /// Explicitly restore REPL state from an opaque snapshot blob.
    pub async fn restore_repl(&mut self, snapshot: &[u8]) -> Result<(), SessionError> {
        let Some(agent) = self.agent.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime agent not available".to_string(),
            ));
        };
        agent.restore(snapshot).await?;
        self.state.repl_snapshot = Some(snapshot.to_vec());
        Ok(())
    }

    /// Run a single turn and stream events to the host sink.
    pub async fn stream_turn(
        &mut self,
        input: TurnInput,
        events: &dyn EventSink,
        cancel: CancellationToken,
    ) -> Result<AssembledTurn, RuntimeError> {
        let normalized = match self.normalize_input_items(&input.items, &input.image_blobs) {
            Ok(items) => items,
            Err(e) => {
                let mut assembler = TurnAssembler::default();
                let error_event = AgentEvent::Error {
                    message: e.clone(),
                    envelope: Some(crate::agent::ErrorEnvelope {
                        kind: "input_validation".to_string(),
                        code: Some("invalid_turn_input".to_string()),
                        user_message: e,
                        raw: None,
                    }),
                };
                assembler.push(&error_event);
                events.emit(error_event).await;
                assembler.push(&AgentEvent::Done);
                events.emit(AgentEvent::Done).await;
                return Ok(assembler.finish(
                    self.state.clone(),
                    false,
                    None,
                    &self.sanitizer,
                    &self.termination,
                ));
            }
        };

        let mut messages = self.state.messages.clone();
        let mode = input.mode.unwrap_or(match self.host_profile {
            HostProfile::Interactive | HostProfile::Headless | HostProfile::Embedded => {
                RunMode::Normal
            }
        });
        let plan_file = input
            .plan_file
            .clone()
            .unwrap_or_else(|| ".lash_plan".into());
        let mode_msg = match mode {
            RunMode::Plan => Some(format!(
                "## Plan Mode\n\n\
                You are in PLAN mode. Think, explore, and design — do NOT execute changes.\n\n\
                **Rules:**\n\
                - READ-ONLY: Do not modify project files or run destructive commands\n\
                - You MAY use: read_file, glob, grep, ls, web_search, fetch_url, agent_call\n\
                - You MUST NOT use: edit_file, find_replace, write_file (except the plan file), or shell with write commands\n\
                - Exception: Write your plan to `{}` using write_file\n\n\
                **Workflow:**\n\
                1. Understand the request and note key assumptions\n\
                2. Explore the codebase — read files, search for patterns, understand architecture\n\
                3. Design your approach — consider tradeoffs, identify risks\n\
                4. Write a clear, step-by-step plan to the plan file\n\n\
                When the user switches back to normal mode, you will execute the plan.",
                plan_file
            )),
            RunMode::Normal => input.plan_file.as_ref().and_then(|path| {
                let plan_content = std::fs::read_to_string(path).unwrap_or_default();
                if plan_content.is_empty() {
                    None
                } else {
                    let mut available = Vec::new();
                    if self.capabilities.enabled(CapabilityId::History) {
                        available.push(
                            "- `_history.search(\"query\", mode=\"hybrid\")` — search planning exploration"
                                .to_string(),
                        );
                        available.push(
                            "- `_history.user_messages()` — original user requests".to_string(),
                        );
                    }
                    if self.capabilities.enabled(CapabilityId::Memory) {
                        available
                            .push("- `_mem` — persistent memory (fully preserved)".to_string());
                    }
                    let available_context = if available.is_empty() {
                        "No persisted planning context capabilities are enabled for this agent."
                            .to_string()
                    } else {
                        available.join("\n")
                    };
                    Some(format!(
                        "## Executing Plan\n\n\
                        You are executing a plan from a previous planning session. \
                        Use available context from this agent configuration.\n\n\
                        **Plan file:** `{}`\n\n\
                        ---\n{}\n---\n\n\
                        **Available context:**\n\
                        {}\n\n\
                        Execute the plan step by step.",
                        path, plan_content, available_context
                    ))
                }
            }),
        };
        if let Some(content) = mode_msg {
            let sys_id = format!("m{}", messages.len());
            messages.push(Message {
                id: sys_id.clone(),
                role: MessageRole::System,
                parts: vec![Part {
                    id: format!("{}.p0", sys_id),
                    kind: PartKind::Text,
                    content,
                    tool_call_id: None,
                    tool_name: None,
                    prune_state: PruneState::Intact,
                }],
            });
        }

        let user_id = format!("m{}", messages.len());
        let mut user_parts: Vec<Part> = Vec::new();
        let mut user_images_png: Vec<Vec<u8>> = Vec::new();
        for item in normalized {
            match item {
                NormalizedItem::Text(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    user_parts.push(Part {
                        id: format!("{}.p{}", user_id, user_parts.len()),
                        kind: PartKind::Text,
                        content: text,
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    });
                }
                NormalizedItem::Image(bytes) => {
                    let image_idx = user_images_png.len();
                    user_images_png.push(bytes);
                    user_parts.push(Part {
                        id: format!("{}.p{}", user_id, user_parts.len()),
                        kind: PartKind::Text,
                        content: format!("{IMAGE_REF_PREFIX}{image_idx}"),
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    });
                }
            }
        }
        if user_parts.is_empty() {
            user_parts.push(Part {
                id: format!("{}.p0", user_id),
                kind: PartKind::Text,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            });
        }
        messages.push(Message {
            id: user_id.clone(),
            role: MessageRole::User,
            parts: user_parts,
        });

        self.stream_prepared_turn(messages, user_images_png, events, cancel)
            .await
    }

    /// Run a single turn and return only the assembled terminal result.
    pub async fn run_turn_assembled(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
    ) -> Result<AssembledTurn, RuntimeError> {
        self.stream_turn(input, &NoopEventSink, cancel).await
    }

    /// Run a turn using host-prepared message history.
    pub async fn stream_prepared_turn(
        &mut self,
        messages: Vec<Message>,
        images_png: Vec<Vec<u8>>,
        events: &dyn EventSink,
        cancel: CancellationToken,
    ) -> Result<AssembledTurn, RuntimeError> {
        let cancel_state = cancel.clone();
        let mut agent = self
            .agent
            .take()
            .expect("runtime engine agent must be available");
        let run_offset = self.state.iteration;
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(100);
        let run_task = tokio::spawn(async move {
            let (new_messages, new_iteration) = agent
                .run(messages, images_png, event_tx, cancel, run_offset)
                .await;
            (agent, new_messages, new_iteration)
        });

        let mut assembler = TurnAssembler::default();
        while let Some(event) = event_rx.recv().await {
            assembler.push(&event);
            events.emit(event).await;
        }

        let (agent, new_messages, new_iteration) = match run_task.await {
            Ok(v) => v,
            Err(e) => {
                let issue = TurnIssue {
                    kind: "runtime".to_string(),
                    code: Some("run_task_join_failed".to_string()),
                    message: format!("Runtime turn task failed: {e}"),
                };
                return Ok(assembler.finish(
                    self.state.clone(),
                    cancel_state.is_cancelled(),
                    Some(issue),
                    &self.sanitizer,
                    &self.termination,
                ));
            }
        };

        self.agent = Some(agent);
        self.state.messages = new_messages;
        self.state.iteration = new_iteration;
        if assembler.token_usage.total() > 0 {
            self.state.token_usage = assembler.token_usage.clone();
        }

        Ok(assembler.finish(
            self.state.clone(),
            cancel_state.is_cancelled(),
            None,
            &self.sanitizer,
            &self.termination,
        ))
    }
}

impl RuntimeEngine {
    fn normalize_input_items(
        &self,
        items: &[InputItem],
        image_blobs: &HashMap<String, Vec<u8>>,
    ) -> Result<Vec<NormalizedItem>, String> {
        normalize_input_items(
            items,
            image_blobs,
            &self.base_dir,
            self.path_resolver.as_deref(),
        )
    }
}

fn normalize_input_items(
    items: &[InputItem],
    image_blobs: &HashMap<String, Vec<u8>>,
    base_dir: &Path,
    path_resolver: Option<&dyn PathResolver>,
) -> Result<Vec<NormalizedItem>, String> {
    let mut out: Vec<NormalizedItem> = Vec::new();
    for item in items {
        match item {
            InputItem::Text { text } => push_text(&mut out, text.clone()),
            InputItem::FileRef { path } => {
                let abs = resolve_existing_path(path, true, base_dir, path_resolver)?;
                push_text(&mut out, format!("[file: {}]", abs.display()));
            }
            InputItem::DirRef { path } => {
                let abs = resolve_existing_path(path, false, base_dir, path_resolver)?;
                push_text(
                    &mut out,
                    format!(
                        "[directory: {}]",
                        abs.to_string_lossy().trim_end_matches('/')
                    ),
                );
            }
            InputItem::ImageRef { id } => {
                if id.is_empty() {
                    return Err("Invalid image_ref: id cannot be empty".to_string());
                }
                let Some(blob) = image_blobs.get(id) else {
                    return Err(format!("Invalid image_ref: missing blob for id '{id}'"));
                };
                out.push(NormalizedItem::Image(blob.clone()));
            }
            InputItem::SkillRef { name, args } => {
                if name.is_empty() {
                    return Err("Invalid skill_ref: name cannot be empty".to_string());
                }
                let mut marker = format!("[SKILL:{name}]");
                if let Some(args) = args.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    marker.push(' ');
                    marker.push_str(args);
                }
                push_text(&mut out, marker);
            }
        }
    }
    Ok(out)
}

fn push_text(out: &mut Vec<NormalizedItem>, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some(NormalizedItem::Text(last)) = out.last_mut() {
        last.push_str(&text);
    } else {
        out.push(NormalizedItem::Text(text));
    }
}

fn resolve_existing_path(
    path: &str,
    expect_file: bool,
    base_dir: &Path,
    path_resolver: Option<&dyn PathResolver>,
) -> Result<PathBuf, String> {
    if let Some(resolver) = path_resolver {
        return resolver.resolve(path, expect_file, base_dir);
    }
    if path.is_empty() {
        return Err("Path reference cannot be empty".to_string());
    }
    let p = Path::new(path);
    let candidate = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    };
    if !candidate.exists() {
        return Err(format!(
            "Referenced path does not exist: {}",
            candidate.display()
        ));
    }
    if expect_file && !candidate.is_file() {
        return Err(format!(
            "Referenced path is not a file: {}",
            candidate.display()
        ));
    }
    if !expect_file && !candidate.is_dir() {
        return Err(format!(
            "Referenced path is not a directory: {}",
            candidate.display()
        ));
    }
    candidate
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize {}: {e}", candidate.display()))
}

fn sanitize_assistant_output(text: String, policy: &SanitizerPolicy) -> String {
    if !policy.strip_repl_fragments || text.is_empty() {
        return text;
    }
    let stripped = strip_repl_fragments(&text);
    let normalized = stripped
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    normalized.trim().to_string()
}

fn classify_output_state(raw_text: &str, safe_text: &str, issues: &[TurnIssue]) -> OutputState {
    if safe_text.is_empty() && raw_text.is_empty() {
        return OutputState::EmptyOutput;
    }
    if safe_text.is_empty() && contains_traceback_only(raw_text) {
        return OutputState::TracebackOnly;
    }
    if !issues.is_empty() && !safe_text.is_empty() {
        return OutputState::RecoveredFromError;
    }
    if safe_text != raw_text {
        return OutputState::Sanitized;
    }
    OutputState::Usable
}

fn contains_traceback_only(raw_text: &str) -> bool {
    if raw_text.is_empty() {
        return false;
    }
    let has_traceback = raw_text.contains("Traceback (most recent call last)")
        || raw_text.lines().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("Runtime error:")
                || trimmed.starts_with("NameError:")
                || trimmed.starts_with("TypeError:")
                || trimmed.starts_with("ValueError:")
                || trimmed.starts_with("KeyError:")
                || trimmed.starts_with("AttributeError:")
                || trimmed.starts_with("SyntaxError:")
                || trimmed.starts_with("ImportError:")
                || trimmed.starts_with("ModuleNotFoundError:")
        });
    if !has_traceback {
        return false;
    }
    // If no alphabetic prose besides traceback/exception formatting, treat as traceback-only.
    !raw_text.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("Traceback")
            || trimmed.starts_with("File ")
            || trimmed.starts_with("Runtime error:")
        {
            return false;
        }
        !trimmed.contains(':')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_state() -> AgentStateEnvelope {
        AgentStateEnvelope::default()
    }

    #[test]
    fn assembler_prefers_final_message() {
        let mut assembler = TurnAssembler::default();
        assembler.push(&AgentEvent::TextDelta {
            content: "stream".to_string(),
        });
        assembler.push(&AgentEvent::Message {
            text: "final".to_string(),
            kind: "final".to_string(),
        });
        assembler.push(&AgentEvent::Done);
        let out = assembler.finish(
            default_state(),
            false,
            None,
            &SanitizerPolicy::default(),
            &TerminationPolicy::default(),
        );
        assert_eq!(out.status, TurnStatus::Completed);
        assert_eq!(out.done_reason, DoneReason::ModelStop);
        assert_eq!(out.assistant_output.safe_text, "final");
        assert_eq!(out.assistant_output.raw_text, "final");
        assert_eq!(out.assistant_output.state, OutputState::Usable);
    }

    #[test]
    fn assembler_marks_tool_failure() {
        let mut assembler = TurnAssembler::default();
        assembler.push(&AgentEvent::ToolCall {
            name: "x".to_string(),
            args: serde_json::json!({}),
            result: serde_json::json!({"error": true}),
            success: false,
            duration_ms: 1,
        });
        assembler.push(&AgentEvent::Error {
            message: "tool failed".to_string(),
            envelope: None,
        });
        assembler.push(&AgentEvent::Done);
        let out = assembler.finish(
            default_state(),
            false,
            None,
            &SanitizerPolicy::default(),
            &TerminationPolicy::default(),
        );
        assert_eq!(out.status, TurnStatus::Failed);
        assert_eq!(out.done_reason, DoneReason::ToolFailure);
        assert_eq!(out.tool_calls.len(), 1);
    }

    #[test]
    fn assembler_marks_missing_done_as_failure() {
        let mut assembler = TurnAssembler::default();
        assembler.push(&AgentEvent::TextDelta {
            content: "partial".to_string(),
        });
        let out = assembler.finish(
            default_state(),
            false,
            None,
            &SanitizerPolicy::default(),
            &TerminationPolicy::default(),
        );
        assert_eq!(out.status, TurnStatus::Failed);
        assert_eq!(out.done_reason, DoneReason::RuntimeError);
    }

    #[test]
    fn assembler_detects_max_turn_message() {
        let mut state = default_state();
        state.messages.push(Message {
            id: "m0".to_string(),
            role: MessageRole::System,
            parts: vec![Part {
                id: "m0.p0".to_string(),
                kind: PartKind::Text,
                content: "Turn limit reached (5).".to_string(),
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            }],
        });
        let mut assembler = TurnAssembler::default();
        assembler.push(&AgentEvent::Done);
        let out = assembler.finish(
            state,
            false,
            None,
            &SanitizerPolicy::default(),
            &TerminationPolicy::default(),
        );
        assert_eq!(out.status, TurnStatus::Completed);
        assert_eq!(out.done_reason, DoneReason::MaxTurns);
    }

    #[test]
    fn sanitizer_strips_repl_tags() {
        let cleaned = sanitize_assistant_output(
            "<repl>print('x')</repl> done".to_string(),
            &SanitizerPolicy {
                strip_repl_fragments: true,
            },
        );
        assert_eq!(cleaned, "print('x') done");
    }

    #[test]
    fn sanitizer_strips_dangling_repl_fragments() {
        let cleaned = sanitize_assistant_output(
            "status <repl\nstill here </repl".to_string(),
            &SanitizerPolicy {
                strip_repl_fragments: true,
            },
        );
        assert_eq!(cleaned, "status\nstill here");
    }

    #[test]
    fn output_state_empty_output() {
        assert_eq!(classify_output_state("", "", &[]), OutputState::EmptyOutput);
    }

    #[test]
    fn output_state_traceback_only() {
        let raw = "Runtime error: Traceback (most recent call last):\nFile \"repl_1.py\", line 2, in <module>\nNameError: name 'now' is not defined";
        assert_eq!(
            classify_output_state(raw, "", &[]),
            OutputState::TracebackOnly
        );
    }

    #[test]
    fn output_state_sanitized() {
        let raw = "<repl>print('x')</repl> done";
        let safe = "print('x') done";
        assert_eq!(
            classify_output_state(raw, safe, &[]),
            OutputState::Sanitized
        );
    }

    #[test]
    fn output_state_recovered_from_error() {
        let issues = vec![TurnIssue {
            kind: "runtime".to_string(),
            code: Some("example".to_string()),
            message: "something failed".to_string(),
        }];
        assert_eq!(
            classify_output_state("raw", "usable", &issues),
            OutputState::RecoveredFromError
        );
    }

    #[test]
    fn normalize_items_resolves_relative_paths_with_base_dir() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let file_path = tmp.path().join("a.txt");
        std::fs::write(&file_path, "x").expect("write");
        let dir_path = tmp.path().join("sub");
        std::fs::create_dir_all(&dir_path).expect("mkdir");

        let items = vec![
            InputItem::FileRef {
                path: "a.txt".to_string(),
            },
            InputItem::DirRef {
                path: "sub".to_string(),
            },
        ];
        let out =
            normalize_input_items(&items, &HashMap::new(), tmp.path(), None).expect("normalized");
        assert_eq!(out.len(), 1);
        match &out[0] {
            NormalizedItem::Text(text) => {
                assert!(text.contains("[file:"));
                assert!(text.contains("[directory:"));
            }
            _ => panic!("expected merged text item"),
        }
    }
}
