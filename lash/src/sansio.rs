//! Sans-IO state machine for agent turns.
//!
//! `TurnMachine` encapsulates all protocol logic for both NativeTools and REPL
//! execution modes. The host event loop drives the machine by calling
//! `poll_effect()` and feeding responses back via `handle_response()`.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::agent::exec::ExecAccumulator;
use crate::agent::message::IMAGE_REF_PREFIX;
use crate::agent::{
    AgentEvent, ErrorEnvelope, LLM_MAX_RETRIES, LLM_RETRY_DELAYS, Message, MessageRole, Part,
    PartKind, PromptComposeInput, PromptProfile, PromptSectionOverride, PruneState, TokenUsage,
    TurnTerminationPolicyState, apply_context_folding, build_assistant_parts,
    compose_system_prompt, format_tool_result_content, is_malformed_assistant_output,
    log_llm_debug, make_error_event, parse_fence_line, resolve_context_instructions,
    truncate_raw_error,
};
use crate::instructions::InstructionSource;
use crate::llm::types::{
    LlmAttachment, LlmOutputPart, LlmRequest, LlmResponse, LlmToolChoice, LlmToolSpec, LlmUsage,
};
use crate::{ContextFoldingConfig, ExecutionMode, ToolCallRecord, ToolResult};

// ─── Public types ───

/// Opaque identifier linking an effect to its response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EffectId(pub u64);

/// An effect the host must fulfil.
#[derive(Debug)]
pub enum Effect {
    /// Start an LLM call.
    /// For NativeTools the host returns `Response::LlmComplete`.
    /// For REPL the host streams via `handle_llm_delta()` then `Response::LlmComplete`.
    LlmCall { id: EffectId, request: LlmRequest },
    /// Cancel an in-progress LLM stream (REPL: code fence detected mid-stream).
    CancelLlm { id: EffectId },
    /// Execute a native tool call. Host returns `Response::ToolResult`.
    ToolCall {
        id: EffectId,
        call_id: String,
        tool_name: String,
        args: Value,
    },
    /// Execute a REPL code block. Host returns `Response::ExecResult`.
    ExecCode { id: EffectId, code: String },
    /// Retry backoff. Host returns `Response::Timeout`.
    Sleep { id: EffectId, duration: Duration },
    /// Fire-and-forget event (no response needed).
    Emit(AgentEvent),
    /// Turn is done.
    Done {
        messages: Vec<Message>,
        iteration: usize,
    },
}

/// Error details from a failed LLM call.
#[derive(Clone, Debug)]
pub struct LlmCallError {
    pub message: String,
    pub retryable: bool,
    pub raw: Option<String>,
    pub code: Option<String>,
}

/// A response to a previously emitted effect.
pub enum Response {
    /// Full LLM response (NativeTools), or final response after streaming (REPL).
    LlmComplete {
        id: EffectId,
        result: Result<LlmResponse, LlmCallError>,
    },
    /// Native tool result.
    ToolResult {
        id: EffectId,
        call_id: String,
        tool_name: String,
        result: ToolResult,
        duration_ms: u64,
    },
    /// REPL code execution result.
    ExecResult {
        id: EffectId,
        result: Result<crate::ExecResponse, String>,
    },
    /// Sleep completed.
    Timeout { id: EffectId },
}

/// Configuration for a `TurnMachine` instance.
pub struct TurnMachineConfig {
    pub execution_mode: ExecutionMode,
    pub model: String,
    pub context_folding: ContextFoldingConfig,
    pub max_context_tokens: usize,
    pub max_turns: Option<usize>,
    pub headless: bool,
    pub sub_agent: bool,
    pub include_soul: bool,
    pub reasoning_effort: Option<String>,
    pub session_id: Option<String>,
    pub tool_list: String,
    pub tool_specs: Vec<LlmToolSpec>,
    pub tool_names: Vec<String>,
    pub enabled_capability_ids: BTreeSet<String>,
    pub helper_bindings: BTreeSet<String>,
    pub capability_prompt_sections: Vec<String>,
    pub can_write: bool,
    pub history_enabled: bool,
    pub project_instructions: String,
    pub prompt_overrides: Vec<PromptSectionOverride>,
    pub base_context: String,
    pub instruction_source: Arc<dyn InstructionSource>,
    pub llm_log_path: Option<PathBuf>,
    pub agent_id: String,
}

// ─── Internal state ───

#[allow(dead_code)]
struct PendingToolCall {
    call_id: String,
    tool_name: String,
}

#[allow(dead_code)]
struct CompletedToolCall {
    call_id: String,
    tool_name: String,
    args: Value,
    result: ToolResult,
    duration_ms: u64,
}

/// REPL fence parser state.
struct FenceState {
    response: String,
    in_code_fence: bool,
    current_prose: String,
    current_code: String,
    prose_parts: Vec<String>,
    code_parts: Vec<String>,
    last_line_start: usize,
    code_executed: bool,
    acc: ExecAccumulator,
    latest_usage: LlmUsage,
}

impl FenceState {
    fn new() -> Self {
        Self {
            response: String::new(),
            in_code_fence: false,
            current_prose: String::new(),
            current_code: String::new(),
            prose_parts: Vec::new(),
            code_parts: Vec::new(),
            last_line_start: 0,
            code_executed: false,
            acc: ExecAccumulator::new(),
            latest_usage: LlmUsage::default(),
        }
    }

    fn reset_for_retry(&mut self) {
        self.response.clear();
        self.in_code_fence = false;
        self.current_prose.clear();
        self.current_code.clear();
        self.prose_parts.clear();
        self.code_parts.clear();
        self.last_line_start = 0;
        self.code_executed = false;
        self.acc = ExecAccumulator::new();
    }
}

/// Accumulated REPL turn state carried across exec cycles.
#[allow(dead_code)]
struct ReplTurnState {
    fence: FenceState,
    tool_images: Vec<(String, Vec<u8>)>,
}

#[allow(dead_code)]
enum MachineState {
    PrepareIteration,
    WaitingLlm {
        effect_id: EffectId,
        // REPL streaming state (None for NativeTools)
        fence: Option<FenceState>,
        retry_attempt: usize,
        stop_stream_processing: bool,
    },
    WaitingRetry {
        effect_id: EffectId,
        retry_attempt: usize,
        last_error: String,
        /// Saved REPL fence state for retry continuation
        fence: Option<FenceState>,
    },
    WaitingTools {
        pending: HashMap<EffectId, PendingToolCall>,
        completed: Vec<CompletedToolCall>,
        assistant_text: String,
        tool_call_parts: Vec<(String, String, String)>, // (call_id, tool_name, input_json)
    },
    WaitingExec {
        effect_id: EffectId,
        repl: ReplTurnState,
    },
    ProcessReplResult {
        repl: ReplTurnState,
    },
    Finished,
}

/// Sans-IO state machine for a single agent run (multi-turn).
pub struct TurnMachine {
    config: TurnMachineConfig,
    state: MachineState,
    pending_effects: VecDeque<Effect>,
    next_effect_id: u64,
    messages: Vec<Message>,
    /// User-provided images (mime, data).
    user_images: Vec<(String, Vec<u8>)>,
    /// Tool-produced images for the next turn.
    tool_images: Vec<(String, Vec<u8>)>,
    iteration: usize,
    run_offset: usize,
    cumulative_usage: TokenUsage,
    last_input_tokens: usize,
    termination: TurnTerminationPolicyState,
}

impl TurnMachine {
    /// Create a new machine in `PrepareIteration` state.
    pub fn new(
        config: TurnMachineConfig,
        messages: Vec<Message>,
        images: Vec<Vec<u8>>,
        run_offset: usize,
    ) -> Self {
        let user_images: Vec<(String, Vec<u8>)> = images
            .into_iter()
            .map(|png_bytes| ("image/png".to_string(), png_bytes))
            .collect();
        Self {
            config,
            state: MachineState::PrepareIteration,
            pending_effects: VecDeque::new(),
            next_effect_id: 1,
            messages,
            user_images,
            tool_images: Vec::new(),
            iteration: run_offset,
            run_offset,
            cumulative_usage: TokenUsage::default(),
            last_input_tokens: 0,
            termination: TurnTerminationPolicyState::new(),
        }
    }

    /// Whether the machine has finished.
    pub fn is_done(&self) -> bool {
        matches!(self.state, MachineState::Finished)
    }

    fn next_id(&mut self) -> EffectId {
        let id = EffectId(self.next_effect_id);
        self.next_effect_id += 1;
        id
    }

    fn emit(&mut self, event: AgentEvent) {
        self.pending_effects.push_back(Effect::Emit(event));
    }

    fn finish(&mut self) {
        self.emit(AgentEvent::Done);
        let msgs = std::mem::take(&mut self.messages);
        let iteration = self.iteration;
        self.state = MachineState::Finished;
        self.pending_effects.push_back(Effect::Done {
            messages: msgs,
            iteration,
        });
    }

    /// Drain the next pending effect. Returns `None` when the host must call
    /// `handle_response()` before more effects become available.
    pub fn poll_effect(&mut self) -> Option<Effect> {
        // If we have queued effects, drain them first.
        if let Some(effect) = self.pending_effects.pop_front() {
            return Some(effect);
        }

        // Otherwise, advance the state machine.
        match &self.state {
            MachineState::PrepareIteration => {
                self.prepare_iteration();
                self.pending_effects.pop_front()
            }
            _ => None,
        }
    }

    // ─── State transitions ───

    fn prepare_iteration(&mut self) {
        // Context folding
        let fold = apply_context_folding(
            &mut self.messages,
            self.last_input_tokens,
            self.config.max_context_tokens,
            self.config.context_folding,
            self.config.history_enabled,
        );

        let context = self.config.base_context.clone();
        let chat_msgs = crate::agent::messages_to_chat(&self.messages);

        self.emit(AgentEvent::LlmRequest {
            iteration: self.iteration,
            message_count: self.messages.len(),
            tool_list: self.config.tool_list.clone(),
        });

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
            tool_list: &self.config.tool_list,
            tool_names: &self.config.tool_names,
            has_history: fold.has_archived_history,
            enabled_capability_ids: &self.config.enabled_capability_ids,
            helper_bindings: &self.config.helper_bindings,
            capability_prompt_sections: &self.config.capability_prompt_sections,
            can_write: self.config.can_write,
            include_soul,
            project_instructions: &self.config.project_instructions,
            overrides: &self.config.prompt_overrides,
        });

        let all_images: Vec<(String, Vec<u8>)> = self
            .user_images
            .iter()
            .chain(self.tool_images.iter())
            .map(|(mime, data)| (mime.clone(), data.clone()))
            .collect();

        let is_native = matches!(self.config.execution_mode, ExecutionMode::NativeTools);

        let llm_request = LlmRequest {
            model: self.config.model.clone(),
            system_prompt,
            messages: chat_msgs,
            attachments: all_images
                .iter()
                .map(|(mime, data)| LlmAttachment {
                    mime: mime.clone(),
                    data: data.clone(),
                })
                .collect(),
            tools: if is_native {
                self.config.tool_specs.clone()
            } else {
                Vec::new()
            },
            tool_choice: if is_native && !self.config.tool_specs.is_empty() {
                LlmToolChoice::Auto
            } else {
                LlmToolChoice::None
            },
            reasoning_effort: self.config.reasoning_effort.clone(),
            session_id: self.config.session_id.clone(),
            // The host is responsible for wiring up stream_events
            stream_events: None,
        };

        // Clear tool images since they've been included in the request
        self.tool_images.clear();

        let id = self.next_id();
        let fence = if !is_native {
            Some(FenceState::new())
        } else {
            None
        };
        self.state = MachineState::WaitingLlm {
            effect_id: id,
            fence,
            retry_attempt: 0,
            stop_stream_processing: false,
        };
        self.pending_effects.push_back(Effect::LlmCall {
            id,
            request: llm_request,
        });
    }

    /// Feed an incremental LLM text delta (REPL mode only).
    /// Returns `true` if the host should continue streaming, `false` to cancel.
    pub fn handle_llm_delta(&mut self, _id: EffectId, text: &str) -> bool {
        // Extract state we need — avoid holding &mut self.state across pushes to pending_effects
        let (fence, effect_id) = match &mut self.state {
            MachineState::WaitingLlm {
                fence: Some(fence),
                stop_stream_processing,
                effect_id,
                ..
            } => {
                if *stop_stream_processing || text.is_empty() {
                    return !*stop_stream_processing;
                }
                (fence as *mut FenceState, *effect_id)
            }
            _ => return true,
        };

        // SAFETY: We hold &mut self exclusively. The pointer avoids the borrow checker
        // issue of borrowing self.state and self.pending_effects simultaneously.
        // We never invalidate the pointer (no state transitions) until the explicit
        // mem::replace below.
        let fence = unsafe { &mut *fence };

        fence.response.push_str(text);

        let mut queued_effects: Vec<Effect> = Vec::new();
        let mut transition_to_exec: Option<String> = None;

        while let Some(nl) = fence.response[fence.last_line_start..].find('\n') {
            let line_end = fence.last_line_start + nl;
            let line = fence.response[fence.last_line_start..line_end].to_string();
            fence.last_line_start = line_end + 1;

            let parsed = parse_fence_line(
                &line,
                &mut fence.in_code_fence,
                &mut fence.current_prose,
                &mut fence.current_code,
                &mut fence.prose_parts,
            );

            if !parsed.prose_delta.is_empty() {
                queued_effects.push(Effect::Emit(AgentEvent::TextDelta {
                    content: format!("{}\n", parsed.prose_delta),
                }));
            }

            for code in parsed.codes_to_execute {
                fence.code_parts.push(code.clone());
                queued_effects.push(Effect::Emit(AgentEvent::CodeBlock { code: code.clone() }));

                if !fence.acc.had_failure {
                    fence.code_executed = true;
                    transition_to_exec = Some(code);
                    break;
                }
                fence.code_executed = true;
            }

            if transition_to_exec.is_some()
                || fence.code_executed
                || !fence.acc.final_response.is_empty()
            {
                break;
            }
        }

        // Flush queued effects
        for e in queued_effects {
            self.pending_effects.push_back(e);
        }

        if let Some(code) = transition_to_exec {
            let exec_id = self.next_id();
            let fence_taken = match std::mem::replace(&mut self.state, MachineState::Finished) {
                MachineState::WaitingLlm { fence: Some(f), .. } => f,
                _ => unreachable!(),
            };

            self.pending_effects
                .push_back(Effect::CancelLlm { id: effect_id });

            self.state = MachineState::WaitingExec {
                effect_id: exec_id,
                repl: ReplTurnState {
                    fence: fence_taken,
                    tool_images: std::mem::take(&mut self.tool_images),
                },
            };
            self.pending_effects
                .push_back(Effect::ExecCode { id: exec_id, code });
            return false;
        }

        // Mark stop if code was executed
        if let MachineState::WaitingLlm {
            fence: Some(f),
            stop_stream_processing,
            ..
        } = &mut self.state
            && (f.code_executed || !f.acc.final_response.is_empty())
        {
            *stop_stream_processing = true;
            return false;
        }

        true
    }

    /// Feed a streaming usage update.
    pub fn handle_llm_usage(&mut self, _id: EffectId, usage: &LlmUsage) {
        if let MachineState::WaitingLlm {
            fence: Some(fence), ..
        } = &mut self.state
        {
            fence.latest_usage = usage.clone();
        }
    }

    /// Feed a response to a previously emitted effect.
    pub fn handle_response(&mut self, response: Response) {
        match response {
            Response::LlmComplete { id, result } => self.handle_llm_complete(id, result),
            Response::ToolResult {
                id,
                call_id,
                tool_name,
                result,
                duration_ms,
            } => self.handle_tool_result(id, call_id, tool_name, result, duration_ms),
            Response::ExecResult { id, result } => self.handle_exec_result(id, result),
            Response::Timeout { id } => self.handle_timeout(id),
        }
    }

    fn handle_llm_complete(&mut self, _id: EffectId, result: Result<LlmResponse, LlmCallError>) {
        match self.config.execution_mode {
            ExecutionMode::NativeTools => self.handle_native_llm_complete(result),
            ExecutionMode::Repl => self.handle_repl_llm_complete(result),
        }
    }

    // ─── NativeTools path ───

    fn handle_native_llm_complete(&mut self, result: Result<LlmResponse, LlmCallError>) {
        let retry_attempt = match &self.state {
            MachineState::WaitingLlm { retry_attempt, .. } => *retry_attempt,
            _ => 0,
        };

        let llm_response = match result {
            Ok(resp) => resp,
            Err(e) => {
                if e.retryable && retry_attempt < LLM_MAX_RETRIES {
                    let delay = LLM_RETRY_DELAYS[retry_attempt];
                    self.emit(AgentEvent::RetryStatus {
                        wait_seconds: delay.as_secs(),
                        attempt: retry_attempt + 2,
                        max_attempts: LLM_MAX_RETRIES + 1,
                        reason: e.message.clone(),
                    });
                    let sleep_id = self.next_id();
                    self.state = MachineState::WaitingRetry {
                        effect_id: sleep_id,
                        retry_attempt: retry_attempt + 1,
                        last_error: e.message,
                        fence: None,
                    };
                    self.pending_effects.push_back(Effect::Sleep {
                        id: sleep_id,
                        duration: delay,
                    });
                    return;
                }
                self.emit(make_error_event(
                    "llm_provider",
                    e.code.as_deref(),
                    format!("LLM error: {}", e.message),
                    e.raw,
                ));
                self.finish();
                return;
            }
        };

        let usage = TokenUsage {
            input_tokens: llm_response.usage.input_tokens,
            output_tokens: llm_response.usage.output_tokens,
            cached_input_tokens: llm_response.usage.cached_input_tokens,
        };
        self.last_input_tokens = usage.input_tokens as usize;
        self.cumulative_usage.add(&usage);
        self.emit(AgentEvent::TokenUsage {
            iteration: self.iteration,
            usage: usage.clone(),
            cumulative: self.cumulative_usage.clone(),
        });
        log_llm_debug(
            &self.config.llm_log_path,
            &self.config.agent_id,
            self.iteration,
            &usage,
            llm_response.request_body.clone(),
            &llm_response.full_text,
        );

        let response_parts = if llm_response.parts.is_empty() && !llm_response.full_text.is_empty()
        {
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
                        self.emit(AgentEvent::TextDelta {
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
        self.emit(AgentEvent::LlmResponse {
            iteration: self.iteration,
            content: assistant_text.clone(),
            duration_ms: 0, // Host can provide timing via response
        });

        // No tool calls → prose-only → done
        if tool_calls.is_empty() {
            if assistant_text.trim().is_empty() {
                self.emit(make_error_event(
                    "llm_provider",
                    Some("empty_response"),
                    "Model returned no assistant text or tool calls.",
                    None,
                ));
                self.finish();
                return;
            }
            let mid = format!("m{}", self.messages.len());
            self.messages.push(Message {
                id: mid.clone(),
                role: MessageRole::Assistant,
                parts: vec![Part {
                    id: format!("{}.p0", mid),
                    kind: PartKind::Prose,
                    content: assistant_text,
                    tool_call_id: None,
                    tool_name: None,
                    prune_state: PruneState::Intact,
                }],
            });
            self.finish();
            return;
        }

        // Build assistant message with tool call parts
        let asst_id = format!("m{}", self.messages.len());
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

        let mut pending = HashMap::new();
        for (call_id, tool_name, input_json) in &tool_calls {
            assistant_parts.push(Part {
                id: format!("{}.p{}", asst_id, assistant_parts.len()),
                kind: PartKind::ToolCall,
                content: input_json.clone(),
                tool_call_id: Some(call_id.clone()),
                tool_name: Some(tool_name.clone()),
                prune_state: PruneState::Intact,
            });

            let args =
                serde_json::from_str::<Value>(input_json).unwrap_or_else(|_| serde_json::json!({}));
            let effect_id = self.next_id();
            pending.insert(
                effect_id,
                PendingToolCall {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                },
            );
            self.pending_effects.push_back(Effect::ToolCall {
                id: effect_id,
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                args,
            });
        }

        if !assistant_parts.is_empty() {
            self.messages.push(Message {
                id: asst_id,
                role: MessageRole::Assistant,
                parts: assistant_parts,
            });
        }

        self.state = MachineState::WaitingTools {
            pending,
            completed: Vec::new(),
            assistant_text,
            tool_call_parts: tool_calls,
        };
    }

    fn handle_tool_result(
        &mut self,
        id: EffectId,
        call_id: String,
        tool_name: String,
        result: ToolResult,
        duration_ms: u64,
    ) {
        // Emit the event first (borrows self.pending_effects, not self.state)
        let args_val = serde_json::json!({});
        self.emit(AgentEvent::ToolCall {
            name: tool_name.clone(),
            args: args_val.clone(),
            result: result.result.clone(),
            success: result.success,
            duration_ms,
        });

        let all_done = if let MachineState::WaitingTools {
            pending, completed, ..
        } = &mut self.state
        {
            pending.remove(&id);
            completed.push(CompletedToolCall {
                call_id,
                tool_name,
                args: args_val,
                result,
                duration_ms,
            });
            pending.is_empty()
        } else {
            return;
        };

        if all_done {
            self.process_native_tool_results();
        }
    }

    fn process_native_tool_results(&mut self) {
        let (completed, _assistant_text) =
            match std::mem::replace(&mut self.state, MachineState::Finished) {
                MachineState::WaitingTools {
                    completed,
                    assistant_text,
                    ..
                } => (completed, assistant_text),
                _ => unreachable!(),
            };

        let mut result_parts = Vec::new();
        let mut tool_records = Vec::new();

        for outcome in completed {
            tool_records.push(ToolCallRecord {
                tool: outcome.tool_name.clone(),
                args: outcome.args.clone(),
                result: outcome.result.result.clone(),
                success: outcome.result.success,
                duration_ms: outcome.duration_ms,
            });

            result_parts.push(Part {
                id: String::new(),
                kind: PartKind::ToolResult,
                content: format_tool_result_content(outcome.result.success, &outcome.result.result),
                tool_call_id: Some(outcome.call_id.clone()),
                tool_name: Some(outcome.tool_name.clone()),
                prune_state: PruneState::Intact,
            });

            let base_image_idx = self.user_images.len() + self.tool_images.len();
            for (image_offset, image) in outcome.result.images.into_iter().enumerate() {
                self.tool_images
                    .push((image.mime.clone(), image.data.clone()));
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
            let user_id = format!("m{}", self.messages.len());
            for (idx, part) in result_parts.iter_mut().enumerate() {
                part.id = format!("{}.p{}", user_id, idx);
            }
            self.messages.push(Message {
                id: user_id,
                role: MessageRole::User,
                parts: result_parts,
            });
            let context_text = resolve_context_instructions(
                self.config.instruction_source.as_ref(),
                &tool_records,
            );
            if !context_text.is_empty() {
                let instruction_id = format!("m{}", self.messages.len());
                self.messages.push(Message {
                    id: instruction_id.clone(),
                    role: MessageRole::System,
                    parts: vec![Part {
                        id: format!("{}.p0", instruction_id),
                        kind: PartKind::Text,
                        content: context_text,
                        tool_call_id: None,
                        tool_name: None,
                        prune_state: PruneState::Intact,
                    }],
                });
            }
        }

        self.iteration += 1;
        if let Some(max_turns) = self.config.max_turns
            && self.iteration >= self.run_offset + max_turns
        {
            let sys_id = format!("m{}", self.messages.len());
            self.messages.push(Message {
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
            self.finish();
            return;
        }

        self.state = MachineState::PrepareIteration;
    }

    // ─── REPL path ───

    fn handle_repl_llm_complete(&mut self, result: Result<LlmResponse, LlmCallError>) {
        let (fence, retry_attempt, _stop_stream_processing) =
            match std::mem::replace(&mut self.state, MachineState::Finished) {
                MachineState::WaitingLlm {
                    fence,
                    retry_attempt,
                    stop_stream_processing,
                    ..
                } => (fence, retry_attempt, stop_stream_processing),
                other => {
                    self.state = other;
                    return;
                }
            };

        let mut fence = fence.unwrap_or_else(FenceState::new);

        match result {
            Err(e) => {
                if e.retryable && retry_attempt < LLM_MAX_RETRIES {
                    let delay = LLM_RETRY_DELAYS[retry_attempt];
                    self.emit(AgentEvent::RetryStatus {
                        wait_seconds: delay.as_secs(),
                        attempt: retry_attempt + 2,
                        max_attempts: LLM_MAX_RETRIES + 1,
                        reason: e.message.clone(),
                    });
                    let sleep_id = self.next_id();
                    self.state = MachineState::WaitingRetry {
                        effect_id: sleep_id,
                        retry_attempt: retry_attempt + 1,
                        last_error: e.message,
                        fence: Some(fence),
                    };
                    self.pending_effects.push_back(Effect::Sleep {
                        id: sleep_id,
                        duration: delay,
                    });
                    return;
                }
                self.emit(AgentEvent::Error {
                    message: format!("LLM error: {}", e.message),
                    envelope: Some(ErrorEnvelope {
                        kind: "llm_provider".to_string(),
                        code: e.code,
                        user_message: format!("LLM error: {}", e.message),
                        raw: e.raw.map(|s| truncate_raw_error(&s)),
                    }),
                });
                self.finish();
            }
            Ok(llm_response) => {
                // Record usage
                let usage = TokenUsage {
                    input_tokens: llm_response.usage.input_tokens,
                    output_tokens: llm_response.usage.output_tokens,
                    cached_input_tokens: llm_response.usage.cached_input_tokens,
                };
                self.last_input_tokens = usage.input_tokens as usize;
                self.cumulative_usage.add(&usage);
                self.emit(AgentEvent::TokenUsage {
                    iteration: self.iteration,
                    usage: usage.clone(),
                    cumulative: self.cumulative_usage.clone(),
                });
                log_llm_debug(
                    &self.config.llm_log_path,
                    &self.config.agent_id,
                    self.iteration,
                    &usage,
                    llm_response.request_body.clone(),
                    &fence.response,
                );

                // If we already executed code mid-stream, go to processing
                if fence.code_executed || !fence.acc.final_response.is_empty() {
                    self.emit(AgentEvent::LlmResponse {
                        iteration: self.iteration,
                        content: fence.response.clone(),
                        duration_ms: 0,
                    });
                    self.state = MachineState::ProcessReplResult {
                        repl: ReplTurnState {
                            fence,
                            tool_images: std::mem::take(&mut self.tool_images),
                        },
                    };
                    self.process_repl_result();
                    return;
                }

                // Process non-streamed deltas (buffered response)
                if fence.response.is_empty() && !llm_response.full_text.is_empty() {
                    // Apply full text through fence parser
                    for delta in &llm_response.deltas {
                        if delta.is_empty() {
                            continue;
                        }
                        fence.response.push_str(delta);

                        while let Some(nl) = fence.response[fence.last_line_start..].find('\n') {
                            let line_end = fence.last_line_start + nl;
                            let line = fence.response[fence.last_line_start..line_end].to_string();
                            fence.last_line_start = line_end + 1;

                            let parsed = parse_fence_line(
                                &line,
                                &mut fence.in_code_fence,
                                &mut fence.current_prose,
                                &mut fence.current_code,
                                &mut fence.prose_parts,
                            );

                            if !parsed.prose_delta.is_empty() {
                                self.emit(AgentEvent::TextDelta {
                                    content: format!("{}\n", parsed.prose_delta),
                                });
                            }

                            if let Some(code) = parsed.codes_to_execute.into_iter().next() {
                                fence.code_parts.push(code.clone());
                                self.emit(AgentEvent::CodeBlock { code: code.clone() });
                                fence.code_executed = true;

                                // Need to exec code
                                let exec_id = self.next_id();
                                self.emit(AgentEvent::LlmResponse {
                                    iteration: self.iteration,
                                    content: fence.response.clone(),
                                    duration_ms: 0,
                                });
                                self.state = MachineState::WaitingExec {
                                    effect_id: exec_id,
                                    repl: ReplTurnState {
                                        fence,
                                        tool_images: std::mem::take(&mut self.tool_images),
                                    },
                                };
                                self.pending_effects
                                    .push_back(Effect::ExecCode { id: exec_id, code });
                                return;
                            }
                        }
                        if fence.code_executed || !fence.acc.final_response.is_empty() {
                            break;
                        }
                    }

                    if fence.code_executed || !fence.acc.final_response.is_empty() {
                        self.emit(AgentEvent::LlmResponse {
                            iteration: self.iteration,
                            content: fence.response.clone(),
                            duration_ms: 0,
                        });
                        self.state = MachineState::ProcessReplResult {
                            repl: ReplTurnState {
                                fence,
                                tool_images: std::mem::take(&mut self.tool_images),
                            },
                        };
                        self.process_repl_result();
                        return;
                    }

                    if fence.response.is_empty() {
                        fence.response = llm_response.full_text.clone();
                    }
                }

                self.emit(AgentEvent::LlmResponse {
                    iteration: self.iteration,
                    content: if fence.response.is_empty() {
                        llm_response.full_text.clone()
                    } else {
                        fence.response.clone()
                    },
                    duration_ms: 0,
                });

                // Process trailing text
                if fence.last_line_start < fence.response.len() {
                    let trailing = fence.response[fence.last_line_start..].to_string();
                    let parsed = parse_fence_line(
                        &trailing,
                        &mut fence.in_code_fence,
                        &mut fence.current_prose,
                        &mut fence.current_code,
                        &mut fence.prose_parts,
                    );
                    if !parsed.prose_delta.is_empty() {
                        self.emit(AgentEvent::TextDelta {
                            content: parsed.prose_delta,
                        });
                    }
                    if let Some(code) = parsed.codes_to_execute.into_iter().next() {
                        fence.code_parts.push(code.clone());
                        self.emit(AgentEvent::CodeBlock { code: code.clone() });
                        fence.code_executed = true;

                        let exec_id = self.next_id();
                        self.state = MachineState::WaitingExec {
                            effect_id: exec_id,
                            repl: ReplTurnState {
                                fence,
                                tool_images: std::mem::take(&mut self.tool_images),
                            },
                        };
                        self.pending_effects
                            .push_back(Effect::ExecCode { id: exec_id, code });
                        return;
                    }
                }

                // Unclosed code fence at end of response
                if fence.in_code_fence && !fence.current_code.trim().is_empty() {
                    let code = fence.current_code.clone();
                    fence.code_parts.push(code.clone());
                    self.emit(AgentEvent::CodeBlock { code: code.clone() });
                    fence.current_code.clear();
                    fence.code_executed = true;

                    let exec_id = self.next_id();
                    self.state = MachineState::WaitingExec {
                        effect_id: exec_id,
                        repl: ReplTurnState {
                            fence,
                            tool_images: std::mem::take(&mut self.tool_images),
                        },
                    };
                    self.pending_effects
                        .push_back(Effect::ExecCode { id: exec_id, code });
                    return;
                }

                // Flush remaining prose
                let remaining_prose = fence.current_prose.trim().to_string();
                if !remaining_prose.is_empty() {
                    fence.prose_parts.push(remaining_prose);
                    fence.current_prose.clear();
                }

                if fence.response.is_empty() && !llm_response.full_text.is_empty() {
                    fence.response = llm_response.full_text;
                }

                // Check for malformed output
                if is_malformed_assistant_output(&fence.response) {
                    let preview = truncate_raw_error(fence.response.trim());
                    if retry_attempt < LLM_MAX_RETRIES {
                        fence.reset_for_retry();
                        let delay = LLM_RETRY_DELAYS[retry_attempt];
                        self.emit(AgentEvent::RetryStatus {
                            wait_seconds: delay.as_secs(),
                            attempt: retry_attempt + 2,
                            max_attempts: LLM_MAX_RETRIES + 1,
                            reason: "malformed assistant output from model (partial repl fragment)"
                                .to_string(),
                        });
                        let sleep_id = self.next_id();
                        self.state = MachineState::WaitingRetry {
                            effect_id: sleep_id,
                            retry_attempt: retry_attempt + 1,
                            last_error:
                                "malformed assistant output from model (partial repl fragment)"
                                    .to_string(),
                            fence: Some(fence),
                        };
                        self.pending_effects.push_back(Effect::Sleep {
                            id: sleep_id,
                            duration: delay,
                        });
                        return;
                    }
                    self.emit(make_error_event(
                        "llm_provider",
                        Some("malformed_output"),
                        "Model returned malformed output. Use /retry to replay this turn.",
                        Some(preview),
                    ));
                    self.finish();
                    return;
                }

                // Go to result processing
                self.state = MachineState::ProcessReplResult {
                    repl: ReplTurnState {
                        fence,
                        tool_images: std::mem::take(&mut self.tool_images),
                    },
                };
                self.process_repl_result();
            }
        }
    }

    fn handle_exec_result(&mut self, _id: EffectId, result: Result<crate::ExecResponse, String>) {
        let mut repl = match std::mem::replace(&mut self.state, MachineState::Finished) {
            MachineState::WaitingExec { repl, .. } => repl,
            other => {
                self.state = other;
                return;
            }
        };

        match result {
            Ok(r) => {
                for tc in &r.tool_calls {
                    self.emit(AgentEvent::ToolCall {
                        name: tc.tool.clone(),
                        args: tc.args.clone(),
                        result: tc.result.clone(),
                        success: tc.success,
                        duration_ms: tc.duration_ms,
                    });
                }
                if !r.output.is_empty() || r.error.is_some() {
                    self.emit(AgentEvent::CodeOutput {
                        output: r.output.clone(),
                        error: r.error.clone(),
                    });
                }

                repl.fence.acc.tool_calls.extend(r.tool_calls);
                repl.fence.acc.images.extend(r.images);
                if !r.output.is_empty() {
                    repl.fence.acc.combined_output.push_str(&r.output);
                }
                if !r.response.is_empty() {
                    repl.fence.acc.final_response = r.response;
                }
                if let Some(raw_error) = r.error {
                    repl.fence.acc.exec_error = Some(raw_error);
                    repl.fence.acc.had_failure = true;
                }
            }
            Err(e) => {
                self.emit(AgentEvent::CodeOutput {
                    output: String::new(),
                    error: Some(e.clone()),
                });
                repl.fence.acc.exec_error = Some(e);
                repl.fence.acc.had_failure = true;
            }
        }

        // Move to processing the repl result
        self.state = MachineState::ProcessReplResult { repl };
        self.process_repl_result();
    }

    fn process_repl_result(&mut self) {
        let repl = match std::mem::replace(&mut self.state, MachineState::Finished) {
            MachineState::ProcessReplResult { repl } => repl,
            other => {
                self.state = other;
                return;
            }
        };

        let fence = repl.fence;
        let executed_text = &fence.response;
        let has_code = !fence.code_parts.is_empty();

        // Check for empty response with no execution
        if executed_text.trim().is_empty()
            && fence.acc.tool_calls.is_empty()
            && fence.acc.combined_output.is_empty()
            && !fence.acc.had_failure
        {
            self.emit(make_error_event(
                "llm_provider",
                Some("empty_response"),
                "I didn't get a response. Use /retry to replay this turn, or /provider if credentials changed.",
                None,
            ));
            self.finish();
            return;
        }

        // Keep tool images
        let mut next_tool_image_refs: Vec<(usize, String)> = Vec::new();
        let base_image_idx = self.user_images.len();
        for (i, img) in fence.acc.images.iter().enumerate() {
            self.tool_images.push((img.mime.clone(), img.data.clone()));
            next_tool_image_refs.push((base_image_idx + i, img.label.clone()));
        }

        // done() = stop signal
        if !fence.acc.final_response.is_empty() {
            self.emit(AgentEvent::Message {
                text: fence.acc.final_response.clone(),
                kind: "final".to_string(),
            });
            let mid = format!("m{}", self.messages.len());
            let asst_parts = build_assistant_parts(&mid, &fence.prose_parts, &fence.code_parts);
            self.messages.push(Message {
                id: mid,
                role: MessageRole::Assistant,
                parts: asst_parts,
            });
            self.finish();
            return;
        }

        let has_output = !fence.acc.combined_output.is_empty();
        let has_tool_calls = !fence.acc.tool_calls.is_empty();

        // Pure prose response — turn ends
        if !has_code && !has_output && !has_tool_calls && !fence.acc.had_failure {
            let mid = format!("m{}", self.messages.len());
            let asst_parts = build_assistant_parts(&mid, &fence.prose_parts, &fence.code_parts);
            self.messages.push(Message {
                id: mid,
                role: MessageRole::Assistant,
                parts: asst_parts,
            });
            if self.config.headless {
                let sys_id = format!("m{}", self.messages.len());
                let guidance = "Headless mode requires execution via <repl>. \
Prose-only output is not a valid step. Continue with concrete tool execution; call done(...) only from inside <repl> when fully complete.";
                self.messages.push(Message {
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
                if self
                    .termination
                    .record_pure_prose_step(self.config.headless)
                {
                    self.emit(make_error_event(
                        "runtime",
                        Some("headless_prose_only"),
                        "Headless run ended after repeated prose-only responses without execution.",
                        None,
                    ));
                    self.finish();
                    return;
                }
                // Continue to next iteration
                self.state = MachineState::PrepareIteration;
                return;
            }
            self.finish();
            return;
        }
        self.termination.reset_pure_prose_streak();

        // Build feedback parts
        let mut feedback_parts: Vec<Part> = Vec::new();
        for code in &fence.code_parts {
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
            let mut output_text = fence.acc.combined_output.clone();
            if has_tool_calls {
                output_text.push_str(&format!(
                    "\n[{} tool call(s) executed]",
                    fence.acc.tool_calls.len()
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
                content: format!("[{} tool call(s) executed]", fence.acc.tool_calls.len()),
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            });
        }
        if let Some(err) = &fence.acc.exec_error {
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

        // Push assistant message
        let asst_id = format!("m{}", self.messages.len());
        let asst_parts = build_assistant_parts(&asst_id, &fence.prose_parts, &fence.code_parts);
        self.messages.push(Message {
            id: asst_id,
            role: MessageRole::Assistant,
            parts: asst_parts,
        });

        // Push system feedback
        let sys_id = format!("m{}", self.messages.len());
        for (idx, part) in feedback_parts.iter_mut().enumerate() {
            part.id = format!("{}.p{}", sys_id, idx);
        }
        self.messages.push(Message {
            id: sys_id,
            role: MessageRole::System,
            parts: feedback_parts,
        });

        // Context instructions
        let context_text = resolve_context_instructions(
            self.config.instruction_source.as_ref(),
            &fence.acc.tool_calls,
        );
        if !context_text.is_empty() {
            let instruction_id = format!("m{}", self.messages.len());
            self.messages.push(Message {
                id: instruction_id.clone(),
                role: MessageRole::System,
                parts: vec![Part {
                    id: format!("{}.p0", instruction_id),
                    kind: PartKind::Text,
                    content: context_text,
                    tool_call_id: None,
                    tool_name: None,
                    prune_state: PruneState::Intact,
                }],
            });
        }

        self.iteration += 1;
        if self.termination.should_force_exit_after_grace_turn() {
            self.finish();
            return;
        }
        self.termination.maybe_schedule_turn_limit_final(
            self.iteration,
            self.run_offset,
            self.config.max_turns,
            &mut self.messages,
        );

        self.state = MachineState::PrepareIteration;
    }

    fn handle_timeout(&mut self, _id: EffectId) {
        let (_retry_attempt, _last_error, _fence) =
            match std::mem::replace(&mut self.state, MachineState::Finished) {
                MachineState::WaitingRetry {
                    retry_attempt,
                    last_error,
                    fence,
                    ..
                } => (retry_attempt, last_error, fence),
                other => {
                    self.state = other;
                    return;
                }
            };

        // Back to PrepareIteration to retry the LLM call
        self.state = MachineState::PrepareIteration;
    }
}

// ─── Tests ───

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Message, MessageRole, Part, PartKind, PruneState};

    struct NullInstructionSource;
    impl InstructionSource for NullInstructionSource {
        fn system_instructions(&self) -> String {
            String::new()
        }
        fn context_instructions_for_reads(&self, _paths: &[String]) -> String {
            String::new()
        }
    }

    fn test_config(mode: ExecutionMode) -> TurnMachineConfig {
        TurnMachineConfig {
            execution_mode: mode,
            model: "test-model".to_string(),
            context_folding: ContextFoldingConfig::default(),
            max_context_tokens: 200_000,
            max_turns: None,
            headless: false,
            sub_agent: false,
            include_soul: false,
            reasoning_effort: None,
            session_id: None,
            tool_list: String::new(),
            tool_specs: Vec::new(),
            tool_names: Vec::new(),
            enabled_capability_ids: BTreeSet::new(),
            helper_bindings: BTreeSet::new(),
            capability_prompt_sections: Vec::new(),
            can_write: false,
            history_enabled: false,
            project_instructions: String::new(),
            prompt_overrides: Vec::new(),
            base_context: String::new(),
            instruction_source: Arc::new(NullInstructionSource),
            llm_log_path: None,
            agent_id: "test".to_string(),
        }
    }

    fn user_message(content: &str) -> Message {
        Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![Part {
                id: "m0.p0".to_string(),
                kind: PartKind::Text,
                content: content.to_string(),
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            }],
        }
    }

    /// Collect effects until a specific variant is found or exhausted.
    fn drain_effects(machine: &mut TurnMachine) -> Vec<Effect> {
        let mut effects = Vec::new();
        while let Some(effect) = machine.poll_effect() {
            effects.push(effect);
        }
        effects
    }

    fn find_llm_call(effects: &[Effect]) -> Option<&EffectId> {
        effects.iter().find_map(|e| match e {
            Effect::LlmCall { id, .. } => Some(id),
            _ => None,
        })
    }

    fn find_done(effects: &[Effect]) -> Option<(&Vec<Message>, usize)> {
        effects.iter().find_map(|e| match e {
            Effect::Done {
                messages,
                iteration,
            } => Some((messages, *iteration)),
            _ => None,
        })
    }

    // ─── NativeTools tests ───

    #[test]
    fn native_prose_only_response_emits_done() {
        let config = test_config(ExecutionMode::NativeTools);
        let msgs = vec![user_message("hello")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).expect("should emit LlmCall");

        // Respond with prose-only
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Ok(LlmResponse {
                full_text: "Hello there!".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "Hello there!".to_string(),
                }],
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        assert!(find_done(&effects).is_some());
        assert!(machine.is_done());
    }

    #[test]
    fn native_tool_calls_produce_effects_and_loop() {
        let config = test_config(ExecutionMode::NativeTools);
        let msgs = vec![user_message("read file")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).expect("should emit LlmCall");

        // Respond with tool call
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::Text {
                        text: "Let me read that.".to_string(),
                    },
                    LlmOutputPart::ToolCall {
                        call_id: "tc1".to_string(),
                        tool_name: "read_file".to_string(),
                        input_json: r#"{"path":"foo.txt"}"#.to_string(),
                    },
                ],
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        // Should have ToolCall effect
        let tool_effect = effects.iter().find_map(|e| match e {
            Effect::ToolCall {
                id,
                call_id,
                tool_name,
                ..
            } => Some((*id, call_id.clone(), tool_name.clone())),
            _ => None,
        });
        assert!(tool_effect.is_some());
        let (tool_id, call_id, tool_name) = tool_effect.unwrap();

        // Feed tool result
        machine.handle_response(Response::ToolResult {
            id: tool_id,
            call_id,
            tool_name,
            result: crate::ToolResult::ok(serde_json::json!("file contents")),
            duration_ms: 10,
        });

        // Should loop back to PrepareIteration → new LlmCall
        let effects = drain_effects(&mut machine);
        let llm_id2 = find_llm_call(&effects);
        assert!(
            llm_id2.is_some(),
            "should emit another LlmCall after tool results"
        );

        // Respond with prose to end
        machine.handle_response(Response::LlmComplete {
            id: *llm_id2.unwrap(),
            result: Ok(LlmResponse {
                full_text: "Done.".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "Done.".to_string(),
                }],
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        assert!(find_done(&effects).is_some());
    }

    #[test]
    fn native_retryable_error_sleeps_then_retries() {
        let config = test_config(ExecutionMode::NativeTools);
        let msgs = vec![user_message("hello")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        // Retryable error
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Err(LlmCallError {
                message: "rate limited".to_string(),
                retryable: true,
                raw: None,
                code: None,
            }),
        });

        let effects = drain_effects(&mut machine);
        let sleep_effect = effects.iter().find_map(|e| match e {
            Effect::Sleep { id, .. } => Some(*id),
            _ => None,
        });
        assert!(sleep_effect.is_some(), "should emit Sleep for retry");

        // Feed timeout
        machine.handle_response(Response::Timeout {
            id: sleep_effect.unwrap(),
        });

        // Should get new LlmCall
        let effects = drain_effects(&mut machine);
        assert!(find_llm_call(&effects).is_some());
    }

    #[test]
    fn native_fatal_error_emits_done() {
        let config = test_config(ExecutionMode::NativeTools);
        let msgs = vec![user_message("hello")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Err(LlmCallError {
                message: "auth failed".to_string(),
                retryable: false,
                raw: None,
                code: None,
            }),
        });

        let effects = drain_effects(&mut machine);
        assert!(find_done(&effects).is_some());
        assert!(machine.is_done());
    }

    #[test]
    fn native_empty_response_emits_error() {
        let config = test_config(ExecutionMode::NativeTools);
        let msgs = vec![user_message("hello")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Ok(LlmResponse::default()),
        });

        let effects = drain_effects(&mut machine);
        let has_error = effects
            .iter()
            .any(|e| matches!(e, Effect::Emit(AgentEvent::Error { .. })));
        assert!(has_error);
        assert!(find_done(&effects).is_some());
    }

    #[test]
    fn native_max_turns_stops_iteration() {
        let mut config = test_config(ExecutionMode::NativeTools);
        config.max_turns = Some(1);
        let msgs = vec![user_message("hello")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        // Respond with tool call to trigger iteration
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "tc1".to_string(),
                    tool_name: "test".to_string(),
                    input_json: "{}".to_string(),
                }],
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        let tool_id = effects
            .iter()
            .find_map(|e| match e {
                Effect::ToolCall { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap();

        machine.handle_response(Response::ToolResult {
            id: tool_id,
            call_id: "tc1".to_string(),
            tool_name: "test".to_string(),
            result: crate::ToolResult::ok(serde_json::json!("ok")),
            duration_ms: 1,
        });

        let effects = drain_effects(&mut machine);
        assert!(find_done(&effects).is_some());
        assert!(machine.is_done());
    }

    // ─── REPL tests ───

    #[test]
    fn repl_prose_only_response_emits_done() {
        let config = test_config(ExecutionMode::Repl);
        let msgs = vec![user_message("hello")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Ok(LlmResponse {
                full_text: "Hello there!".to_string(),
                deltas: vec!["Hello there!".to_string()],
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        assert!(find_done(&effects).is_some());
        assert!(machine.is_done());
    }

    #[test]
    fn repl_code_block_triggers_exec() {
        let config = test_config(ExecutionMode::Repl);
        let msgs = vec![user_message("run some code")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        // Stream deltas with a code block
        let cont =
            machine.handle_llm_delta(llm_id, "Here's the code:\n<repl>\nprint('hi')\n</repl>\n");
        assert!(!cont, "should signal to cancel stream after code block");

        let effects = drain_effects(&mut machine);
        let exec_effect = effects.iter().find_map(|e| match e {
            Effect::ExecCode { id, code } => Some((*id, code.clone())),
            _ => None,
        });
        assert!(exec_effect.is_some(), "should emit ExecCode");
        let (exec_id, code) = exec_effect.unwrap();
        assert_eq!(code, "print('hi')");

        // Feed exec result with done()
        machine.handle_response(Response::ExecResult {
            id: exec_id,
            result: Ok(crate::ExecResponse {
                output: "hi\n".to_string(),
                response: "done".to_string(),
                tool_calls: Vec::new(),
                images: Vec::new(),
                error: None,
                duration_ms: 5,
            }),
        });

        // Now feed the LlmComplete since stream was cancelled
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Ok(LlmResponse {
                usage: LlmUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cached_input_tokens: 0,
                },
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        assert!(find_done(&effects).is_some());
    }

    #[test]
    fn repl_exec_error_produces_feedback() {
        let config = test_config(ExecutionMode::Repl);
        let msgs = vec![user_message("run code")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        // Feed deltas with code block via buffered response
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Ok(LlmResponse {
                full_text: "<repl>\nbad_code()\n</repl>\n".to_string(),
                deltas: vec!["<repl>\nbad_code()\n</repl>\n".to_string()],
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        let exec_effect = effects.iter().find_map(|e| match e {
            Effect::ExecCode { id, .. } => Some(*id),
            _ => None,
        });
        assert!(exec_effect.is_some());

        // Return error
        machine.handle_response(Response::ExecResult {
            id: exec_effect.unwrap(),
            result: Err("NameError: name 'bad_code' is not defined".to_string()),
        });

        let effects = drain_effects(&mut machine);
        // Should have error event and loop to next iteration (not done, since there was output)
        let has_llm_call = effects.iter().any(|e| matches!(e, Effect::LlmCall { .. }));
        assert!(
            has_llm_call,
            "should loop to next iteration after exec error"
        );
    }

    #[test]
    fn repl_malformed_output_retries() {
        let config = test_config(ExecutionMode::Repl);
        let msgs = vec![user_message("hello")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        // Malformed output
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            result: Ok(LlmResponse {
                full_text: "<repl>".to_string(),
                deltas: vec!["<repl>".to_string()],
                ..LlmResponse::default()
            }),
        });

        let effects = drain_effects(&mut machine);
        let has_sleep = effects.iter().any(|e| matches!(e, Effect::Sleep { .. }));
        assert!(has_sleep, "should retry with sleep on malformed output");
    }

    #[test]
    fn repl_mid_stream_cancel_when_fence_closes() {
        let config = test_config(ExecutionMode::Repl);
        let msgs = vec![user_message("code")];
        let mut machine = TurnMachine::new(config, msgs, Vec::new(), 0);

        let effects = drain_effects(&mut machine);
        let llm_id = *find_llm_call(&effects).unwrap();

        // Stream: first delta opens fence
        let cont = machine.handle_llm_delta(llm_id, "<repl>\n");
        assert!(cont, "should continue while fence is open");

        // Stream: code line
        let cont = machine.handle_llm_delta(llm_id, "x = 42\n");
        assert!(cont, "should continue with code");

        // Stream: close fence
        let cont = machine.handle_llm_delta(llm_id, "</repl>\n");
        assert!(!cont, "should cancel after fence closes");

        let effects = drain_effects(&mut machine);
        let has_cancel = effects
            .iter()
            .any(|e| matches!(e, Effect::CancelLlm { .. }));
        let has_exec = effects.iter().any(|e| matches!(e, Effect::ExecCode { .. }));
        assert!(has_cancel, "should emit CancelLlm");
        assert!(has_exec, "should emit ExecCode");
    }
}
