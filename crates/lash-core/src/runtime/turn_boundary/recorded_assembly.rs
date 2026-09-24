//! The committed content of one turn, derived from recorded state (ADR 0105
//! §1; FIG-3672 P6).
//!
//! A turn commits its tool calls, usage, omitted-call summary, issues,
//! cancellation and outcome. The driver records each of them here, in its own
//! program order, from the turn machine's emissions and the recorded outcomes
//! it hands the machine; the turn loop records the terminal sequences it
//! writes itself. None of it is read back from the observation stream the
//! host receives, so how and when observations are published cannot change
//! what a turn commits, and a replay rebuilds the same content from the same
//! recorded outcomes.

use crate::ToolCallRecord;
use crate::session_model::{MessageRole, PartKind, SessionStreamEvent, TokenUsage};
use crate::{TurnFinish, TurnOutcome, TurnStop};

use crate::runtime::{
    AssembledTurn, AssistantOutput, OutputState, TerminationPolicy, TurnExecutionMetrics, TurnIssue,
};

/// The committed content of one turn, folded in the driver's program order.
pub struct RecordedTurnAssembly {
    pub(in crate::runtime) tool_calls: Vec<ToolCallRecord>,
    pub(in crate::runtime) had_code_execution: bool,
    pub(in crate::runtime) omitted: Option<crate::OmittedToolCalls>,
    pub(in crate::runtime) llm_calls: Vec<crate::LlmCallRecord>,
    /// Leading `llm_calls` whose counted response usage `token_usage` already
    /// carries: one `TokenUsage` event fires per counted response, and an
    /// uncounted call can only trail the last counted one because a failed or
    /// discarded response ends the call sequence.
    pub(in crate::runtime) usage_counted_calls: usize,
    pub(in crate::runtime) failure_evidence: Vec<crate::TurnFailureEvidence>,
    pub(in crate::runtime) token_usage: TokenUsage,
    pub(in crate::runtime) last_llm_usage: Option<TokenUsage>,
    pub(in crate::runtime) issues: Vec<TurnIssue>,
    pub(in crate::runtime) saw_done: bool,
    pub(in crate::runtime) outcome: Option<TurnOutcome>,
}

impl Default for RecordedTurnAssembly {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordedTurnAssembly {
    pub(in crate::runtime) fn new() -> Self {
        Self {
            tool_calls: Vec::new(),
            had_code_execution: false,
            omitted: None,
            llm_calls: Vec::new(),
            usage_counted_calls: 0,
            failure_evidence: Vec::new(),
            token_usage: TokenUsage::default(),
            last_llm_usage: None,
            issues: Vec::new(),
            saw_done: false,
            outcome: None,
        }
    }

    /// Record that the machine ran a code cell this turn.
    pub fn note_code_execution(&mut self) {
        self.had_code_execution = true;
    }

    /// Fold one event the driver produced — a turn-machine emission, or a
    /// terminal event the driver or turn loop writes itself — in program
    /// order. Events that carry no committed content are ignored.
    pub fn record(&mut self, event: &SessionStreamEvent) {
        match event {
            SessionStreamEvent::ToolCall {
                call_id,
                name,
                args,
                output,
                duration_ms,
            } => {
                self.tool_calls.push(ToolCallRecord {
                    call_id: call_id.clone(),
                    tool: name.clone(),
                    args: args.clone(),
                    output: output.clone(),
                    duration_ms: *duration_ms,
                });
            }
            SessionStreamEvent::ToolCallsOmitted { summary } => {
                let omitted = self.omitted.get_or_insert_with(|| crate::OmittedToolCalls {
                    count: 0,
                    failures: 0,
                    attachments: Vec::new(),
                });
                omitted.count = omitted.count.saturating_add(summary.count);
                omitted.failures = omitted.failures.saturating_add(summary.failures);
                omitted.attachments.extend(summary.attachments.clone());
            }
            SessionStreamEvent::TokenUsage {
                usage, cumulative, ..
            } => {
                self.token_usage = cumulative.clone();
                self.last_llm_usage = Some(usage.clone());
                self.usage_counted_calls = self.usage_counted_calls.saturating_add(1);
            }
            SessionStreamEvent::Error { message, envelope } => {
                let issue = if let Some(envelope) = envelope {
                    TurnIssue {
                        severity: crate::runtime::TurnIssueSeverity::Blocking,
                        kind: envelope.kind.clone(),
                        code: envelope.code.clone(),
                        terminal_reason: envelope.terminal_reason,
                        message: message.clone(),
                        raw: envelope.raw.clone(),
                        retryable: envelope.retryable,
                        provider_failure_kind: envelope.provider_failure_kind,
                    }
                } else {
                    TurnIssue {
                        severity: crate::runtime::TurnIssueSeverity::Blocking,
                        kind: crate::TurnFailureKind::Runtime,
                        code: None,
                        terminal_reason: None,
                        message: message.clone(),
                        raw: None,
                        retryable: None,
                        provider_failure_kind: None,
                    }
                };
                self.issues.push(issue);
            }
            SessionStreamEvent::Done => {
                self.saw_done = true;
            }
            SessionStreamEvent::TurnOutcome { outcome } => {
                self.outcome = Some(outcome.clone());
            }
            _ => {}
        }
    }

    pub(in crate::runtime) fn with_llm_calls(
        mut self,
        llm_calls: Vec<crate::LlmCallRecord>,
    ) -> Self {
        self.llm_calls = llm_calls;
        self
    }

    pub(in crate::runtime) fn with_failure_evidence(
        mut self,
        failure_evidence: Vec<crate::TurnFailureEvidence>,
    ) -> Self {
        self.failure_evidence = failure_evidence;
        self
    }

    pub fn finish(
        mut self,
        state: crate::SessionSnapshot,
        cancellation: Option<crate::TurnCancellationEvidence>,
        force_runtime_error: Option<TurnIssue>,
        termination: &TerminationPolicy,
    ) -> AssembledTurn {
        let mut issues = self.issues;
        if let Some(issue) = force_runtime_error {
            issues.push(issue);
        }

        let raw_output = if let Some(output) =
            self.outcome.as_ref().and_then(render_outcome_for_output)
        {
            output
        } else {
            let recovered = match recovered_assistant_output_from_state(&state) {
                Ok(recovered) => recovered,
                Err(error) => {
                    issues.push(TurnIssue {
                        severity: crate::runtime::TurnIssueSeverity::Blocking,
                        kind: crate::TurnFailureKind::Runtime,
                        code: Some(crate::TurnFailureCode::SessionGraphScope.into()),
                        terminal_reason: None,
                        message: error.to_string(),
                        raw: None,
                        retryable: Some(false),
                        provider_failure_kind: None,
                    });
                    String::new()
                }
            };
            if !recovered.is_empty() {
                issues.push(TurnIssue {
                    severity: crate::runtime::TurnIssueSeverity::Advisory,
                    kind: crate::TurnFailureKind::Runtime,
                    code: Some(crate::TurnFailureCode::AssistantOutputRecoveredFromState.into()),
                    terminal_reason: None,
                    message: "assistant output was recovered from persisted messages because no explicit assistant output was assembled".to_string(),
                    raw: None,
                    retryable: None,
                    provider_failure_kind: None,
                });
            }
            recovered
        };
        let safe_output = sanitize_assistant_output(raw_output.clone());

        let outcome = if let Some(evidence) = cancellation {
            TurnOutcome::Stopped(TurnStop::Cancelled { evidence })
        } else if let Some(outcome) = self.outcome.take() {
            match outcome {
                TurnOutcome::Finished(TurnFinish::AssistantMessage { .. }) => {
                    TurnOutcome::Finished(TurnFinish::AssistantMessage {
                        text: safe_output.clone(),
                    })
                }
                outcome => outcome,
            }
        } else if !self.saw_done && termination.treat_missing_done_as_failure {
            issues.push(TurnIssue {
                severity: crate::runtime::TurnIssueSeverity::Blocking,
                kind: crate::TurnFailureKind::Runtime,
                code: Some(crate::TurnFailureCode::MissingDone.into()),
                terminal_reason: None,
                message: "turn stream ended without a Done event".to_string(),
                raw: None,
                retryable: None,
                provider_failure_kind: None,
            });
            TurnOutcome::Stopped(TurnStop::RuntimeError)
        } else if issues
            .iter()
            .any(|issue| issue.severity == crate::runtime::TurnIssueSeverity::Blocking)
        {
            if self
                .tool_calls
                .iter()
                .any(|record| !record.output.is_success())
                || self
                    .omitted
                    .as_ref()
                    .is_some_and(|omitted| omitted.failures > 0)
            {
                TurnOutcome::Stopped(TurnStop::ToolFailure)
            } else {
                TurnOutcome::Stopped(TurnStop::RuntimeError)
            }
        } else {
            TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: safe_output.clone(),
            })
        };
        let output_state = classify_output_state(&raw_output, &safe_output, &issues);

        AssembledTurn {
            execution: TurnExecutionMetrics {
                had_tool_calls: !self.tool_calls.is_empty(),
                had_code_execution: self.had_code_execution,
                // Timing is stamped by the turn loop, which owns the
                // claim → final-commit measurement window.
                started_at_ms: 0,
                duration_ms: 0,
            },
            state,
            outcome,
            assistant_output: AssistantOutput {
                safe_text: safe_output,
                raw_text: raw_output,
                state: output_state,
            },
            token_usage: self.token_usage,
            llm_calls: self.llm_calls,
            tool_calls: self.tool_calls,
            omitted: self.omitted,
            failure_evidence: self.failure_evidence,
            errors: issues,
            // Stamped by the ingress that accepted this turn's input, which
            // owns the acceptance identity assembly never sees.
            turn_input_acceptance: None,
            turn_cancel_input_outcome: Default::default(),
        }
    }

    pub(in crate::runtime) fn last_llm_usage(&self) -> Option<&TokenUsage> {
        self.last_llm_usage.as_ref()
    }
}

fn render_final_value_for_output(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn render_outcome_for_output(outcome: &TurnOutcome) -> Option<String> {
    match outcome {
        TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) => Some(text.clone()),
        TurnOutcome::Finished(TurnFinish::FinalValue { value })
        | TurnOutcome::Finished(TurnFinish::ToolValue { value, .. })
        | TurnOutcome::Stopped(TurnStop::SubmittedError { value })
        | TurnOutcome::Stopped(TurnStop::ToolError { value, .. }) => {
            Some(render_final_value_for_output(value))
        }
        TurnOutcome::AgentFrameSwitch { .. }
        | TurnOutcome::Queued { .. }
        | TurnOutcome::Stopped(
            TurnStop::Cancelled { .. }
            | TurnStop::Incomplete
            | TurnStop::InvalidInput
            | TurnStop::MaxTurns
            | TurnStop::ToolFailure
            | TurnStop::ProviderError
            | TurnStop::ContextOverflow
            | TurnStop::PluginAbort
            | TurnStop::RuntimeError,
        ) => None,
    }
}

fn recovered_assistant_output_from_state(
    state: &crate::SessionSnapshot,
) -> Result<String, crate::SessionGraphScopeError> {
    let read_model = state.read_model()?;
    let messages = read_model.messages.as_slice();
    let latest_user_message_idx = messages
        .iter()
        .rposition(|message| matches!(message.role, MessageRole::User));
    let search_messages = latest_user_message_idx
        .map(|idx| &messages[idx.saturating_add(1)..])
        .unwrap_or(messages);
    Ok(search_messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| {
            message
                .parts
                .iter()
                .filter(|part| {
                    matches!(
                        part.kind(),
                        PartKind::Text | PartKind::Prose | PartKind::Attachment
                    )
                })
                .map(|part| part.content())
                .collect::<String>()
        })
        .unwrap_or_default())
}

fn sanitize_assistant_output(text: String) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join(
            "
",
        )
        .trim()
        .to_string()
}

pub fn classify_output_state(raw_text: &str, safe_text: &str, issues: &[TurnIssue]) -> OutputState {
    if safe_text.is_empty() && raw_text.is_empty() {
        return OutputState::EmptyOutput;
    }
    if safe_text.is_empty() && contains_traceback_only(raw_text) {
        return OutputState::TracebackOnly;
    }
    if issues
        .iter()
        .any(|issue| issue.severity == crate::runtime::TurnIssueSeverity::Blocking)
        && !safe_text.is_empty()
    {
        return OutputState::RecoveredFromError;
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
mod tests;
