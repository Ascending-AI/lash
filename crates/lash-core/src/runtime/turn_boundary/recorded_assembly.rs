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

use crate::runtime::{AssembledTurn, TerminationPolicy, TurnExecutionMetrics, TurnIssue};

/// The committed content of one turn, folded in the driver's program order.
pub struct RecordedTurnAssembly {
    pub(in crate::runtime) tool_calls: Vec<ToolCallRecord>,
    pub(in crate::runtime) had_code_execution: bool,
    pub(in crate::runtime) omitted: Option<crate::OmittedToolCalls>,
    /// Outputs the turn's code cells retained out of history (FIG-1643), in
    /// cell order, from each cell's recorded response.
    pub(in crate::runtime) retained_outputs: Vec<crate::RetainedOutput>,
    pub(in crate::runtime) llm_calls: Vec<crate::LlmCallRecord>,
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
            retained_outputs: Vec::new(),
            llm_calls: Vec::new(),
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

    /// Record the outputs a code cell's recorded response retained out of
    /// history: its prints and its finish value (FIG-1643). History holds
    /// only their witnesses and references, inside protocol records the
    /// commit cannot read, so the commit names them from here.
    pub fn note_code_outputs(&mut self, response: &crate::ExecResponse) {
        self.retained_outputs.extend(
            response
                .output_archive
                .as_ref()
                .into_iter()
                .chain(response.terminal_finish_retained.as_ref())
                .cloned(),
        );
    }

    /// Fold one event the driver produced — a turn-machine emission, or a
    /// terminal event the driver or turn loop writes itself — in program
    /// order. Events that carry no committed content are ignored.
    pub fn record(&mut self, event: &SessionStreamEvent) {
        match event {
            SessionStreamEvent::ToolCall {
                call_id,
                provider_call_id,
                name,
                args,
                output,
            } => {
                self.tool_calls.push(ToolCallRecord {
                    call_id: call_id.clone(),
                    provider_call_id: provider_call_id.clone(),
                    tool: name.clone(),
                    args: args.clone(),
                    output: output.clone(),
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
            }
            SessionStreamEvent::Error { message, envelope } => {
                let issue = if let Some(envelope) = envelope {
                    let provider_error = envelope.kind == crate::TurnFailureKind::LlmProvider
                        && envelope.terminal_reason.is_some();
                    TurnIssue {
                        severity: crate::runtime::TurnIssueSeverity::Blocking,
                        kind: envelope.kind.clone(),
                        code: envelope.code.clone(),
                        terminal_reason: envelope.terminal_reason,
                        message: if provider_error {
                            "provider call failed".to_string()
                        } else {
                            envelope.user_message.clone()
                        },
                        raw: if provider_error {
                            None
                        } else {
                            envelope.raw.clone()
                        },
                        retryable: envelope.retryable,
                        provider_failure_kind: envelope.provider_failure_kind,
                        plugin_failures: Vec::new(),
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
                        plugin_failures: Vec::new(),
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

        let outcome = if let Some(evidence) = cancellation {
            TurnOutcome::Stopped(TurnStop::Cancelled { evidence })
        } else if let Some(outcome) = self.outcome.take() {
            outcome
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
                plugin_failures: Vec::new(),
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
            let recovered = recovered_assistant_output_from_state(&state);
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
                    plugin_failures: Vec::new(),
                });
            }
            TurnOutcome::Finished(TurnFinish::AssistantMessage { text: recovered })
        };

        AssembledTurn {
            execution: TurnExecutionMetrics {
                had_tool_calls: !self.tool_calls.is_empty(),
                had_code_execution: self.had_code_execution,
                // Timing is stamped by the turn loop, which owns the
                // admission → final-commit measurement window.
                started_at_ms: 0,
                duration_ms: 0,
            },
            state,
            outcome,
            token_usage: self.token_usage,
            llm_calls: self.llm_calls,
            tool_calls: self.tool_calls,
            omitted: self.omitted,
            retained_outputs: self.retained_outputs,
            failure_evidence: self.failure_evidence,
            errors: issues,
            // Stamped by the ingress that accepted this turn's input, which
            // owns the acceptance identity assembly never sees.
            turn_input_acceptance: None,
            turn_cancel_input_outcome: Default::default(),
        }
    }
}

fn recovered_assistant_output_from_state(state: &crate::SessionSnapshot) -> String {
    let read_model = state.read_model();
    let messages = read_model.messages.as_slice();
    let latest_user_message_idx = messages
        .iter()
        .rposition(|message| matches!(message.role, MessageRole::User));
    let search_messages = latest_user_message_idx
        .map(|idx| &messages[idx.saturating_add(1)..])
        .unwrap_or(messages);
    search_messages
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
        .unwrap_or_default()
}

#[cfg(test)]
mod provider_failure_tests {
    use super::*;

    #[test]
    fn committed_provider_issue_uses_the_structured_envelope() {
        const SECRET: &str = "api_key= secret Authorization: Basic abc";
        let failure = crate::llm::transport::LlmTransportError::new(SECRET)
            .with_kind(crate::ProviderFailureKind::Http)
            .with_code(crate::FailureCode::provider("rate_limit_exceeded"))
            .with_http_status(429);
        let call_record = crate::provider::synthetic_terminal_call_record(
            crate::LlmCallId("secret-attempt".to_string()),
            crate::AttemptOutcome::Failed,
            &failure,
            true,
            crate::ProtocolPosition::ResponseObserved,
            Vec::new(),
        );
        let journaled = serde_json::to_string(&call_record).expect("serialize journaled attempt");
        assert!(!journaled.contains(SECRET));
        assert!(!journaled.contains("Basic abc"));
        assert!(journaled.contains("provider:rate_limit_exceeded"));

        let mut assembly = RecordedTurnAssembly::new();
        let envelope = crate::session_model::make_error_envelope(
            crate::TurnFailureKind::LlmProvider,
            Some(crate::FailureCode::provider("rate_limit_exceeded")),
            Some(crate::LlmTerminalReason::ProviderError),
            SECRET,
            Some(SECRET.to_string()),
        );
        assembly.record(&SessionStreamEvent::Error {
            message: SECRET.to_string(),
            envelope: Some(envelope),
        });
        let committed =
            serde_json::to_string(&assembly.issues).expect("serialize committed issues");
        assert!(!committed.contains(SECRET));
        assert!(!committed.contains("Basic abc"));
        assert!(committed.contains("provider:rate_limit_exceeded"));
    }
}
