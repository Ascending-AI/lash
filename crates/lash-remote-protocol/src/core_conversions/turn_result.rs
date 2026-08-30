impl From<lash_core::TokenLedgerEntry> for RemoteTokenLedgerEntry {
    fn from(value: lash_core::TokenLedgerEntry) -> Self {
        let lash_core::TokenLedgerEntry {
            source,
            model,
            usage,
        } = value;
        Self {
            source,
            model,
            usage: usage.into(),
        }
    }
}

impl From<RemoteTokenLedgerEntry> for lash_core::TokenLedgerEntry {
    fn from(value: RemoteTokenLedgerEntry) -> Self {
        let RemoteTokenLedgerEntry {
            source,
            model,
            usage,
        } = value;
        Self {
            source,
            model,
            usage: usage.into(),
        }
    }
}

impl RemoteTurnReport {
    pub fn from_core(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        turn: lash_core::facade_support::AssembledTurn,
        activities: impl IntoIterator<Item = RemoteTurnActivity>,
    ) -> Self {
        // `state` is the local session snapshot; it never crosses the wire.
        // `turn_input_acceptance` is durable admission identity a remote host
        // reads from the pending-input and application surfaces, which are
        // already versioned; the turn result does not duplicate it.
        let lash_core::facade_support::AssembledTurn {
            state: _,
            turn_input_acceptance: _,
            turn_cancel_input_outcome: _,
            outcome,
            assistant_output,
            execution,
            token_usage,
            children_usage,
            llm_calls,
            tool_calls,
            // Failure evidence is a durable turn-receipt read surface, not a
            // duplicate remote execution-result payload.
            failure_evidence: _,
            errors,
        } = turn;
        let activities = activities.into_iter().collect::<Vec<_>>();
        let parent = RemoteUsage::from(token_usage);
        let children = children_usage
            .into_iter()
            .map(RemoteTokenLedgerEntry::from)
            .collect::<Vec<_>>();
        let mut total = parent.clone();
        for child in &children {
            total.add(&child.usage);
        }
        // `status` and the top-level `cancellation` field are projections of
        // the outcome; the outcome is the only place either fact is stated.
        let cancellation = outcome
            .cancellation()
            .cloned()
            .map(RemoteTurnCancellationEvidence::from);
        let outcome = RemoteTurnOutcome::from(outcome);
        let status = RemoteTurnStatus::from(&outcome);
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            status,
            outcome,
            cancellation,
            assistant_output: assistant_output.into(),
            usage: RemoteTurnUsageReport {
                parent,
                children,
                total,
            },
            execution: execution.into(),
            tool_calls: tool_calls.into_iter().map(RemoteToolCallRecord::from).collect(),
            llm_calls: llm_calls.into_iter().map(Into::into).collect(),
            issues: errors.into_iter().map(Into::into).collect(),
            activities,
            metadata: HashMap::new(),
        }
    }
}

impl From<lash_core::facade_support::TurnOutcome> for RemoteTurnOutcome {
    fn from(value: lash_core::facade_support::TurnOutcome) -> Self {
        match value {
            lash_core::facade_support::TurnOutcome::Finished(finish) => Self::Finished {
                finish: finish.into(),
            },
            lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
                frame_key, task, ..
            } => {
                // Frame seeds are deliberately omitted from this lean result projection.
                Self::AgentFrameSwitch {
                    frame_key: frame_key.as_str().to_string(),
                    task,
                }
            }
            lash_core::facade_support::TurnOutcome::Stopped(stop) => Self::Stopped { stop: stop.into() },
        }
    }
}

impl From<lash_core::facade_support::TurnFinish> for RemoteTurnFinish {
    fn from(value: lash_core::facade_support::TurnFinish) -> Self {
        match value {
            lash_core::facade_support::TurnFinish::AssistantMessage { text } => Self::AssistantMessage { text },
            lash_core::facade_support::TurnFinish::FinalValue { value } => Self::FinalValue { value },
            lash_core::facade_support::TurnFinish::ToolValue { tool_name, value } => {
                Self::ToolValue { tool_name, value }
            }
        }
    }
}

impl From<lash_core::facade_support::TurnStop> for RemoteTurnStop {
    fn from(value: lash_core::facade_support::TurnStop) -> Self {
        match value {
            lash_core::facade_support::TurnStop::Cancelled { .. } => Self::Cancelled,
            lash_core::facade_support::TurnStop::Incomplete => Self::Incomplete,
            lash_core::facade_support::TurnStop::InvalidInput => Self::InvalidInput,
            lash_core::facade_support::TurnStop::MaxTurns => Self::MaxTurns,
            lash_core::facade_support::TurnStop::ToolFailure => Self::ToolFailure,
            lash_core::facade_support::TurnStop::ProviderError => Self::ProviderError,
            lash_core::facade_support::TurnStop::PluginAbort => Self::PluginAbort,
            lash_core::facade_support::TurnStop::RuntimeError => Self::RuntimeError,
            lash_core::facade_support::TurnStop::SubmittedError { value } => Self::SubmittedError { value },
            lash_core::facade_support::TurnStop::ToolError { tool_name, value } => {
                Self::ToolError { tool_name, value }
            }
        }
    }
}

impl From<lash_core::facade_support::AssistantOutput> for RemoteAssistantOutput {
    fn from(value: lash_core::facade_support::AssistantOutput) -> Self {
        let lash_core::facade_support::AssistantOutput {
            safe_text,
            raw_text,
            state,
        } = value;
        Self {
            safe_text,
            raw_text,
            state: state.into(),
        }
    }
}

impl From<lash_core::facade_support::OutputState> for RemoteAssistantOutputState {
    fn from(value: lash_core::facade_support::OutputState) -> Self {
        match value {
            lash_core::facade_support::OutputState::Usable => Self::Usable,
            lash_core::facade_support::OutputState::EmptyOutput => Self::EmptyOutput,
            lash_core::facade_support::OutputState::TracebackOnly => Self::TracebackOnly,
            lash_core::facade_support::OutputState::RecoveredFromError => Self::RecoveredFromError,
        }
    }
}

impl From<lash_core::facade_support::TurnExecutionMetrics> for RemoteTurnExecutionMetrics {
    fn from(value: lash_core::facade_support::TurnExecutionMetrics) -> Self {
        let lash_core::facade_support::TurnExecutionMetrics {
            had_tool_calls,
            had_code_execution,
            started_at_ms,
            duration_ms,
        } = value;
        Self {
            had_tool_calls,
            had_code_execution,
            started_at_ms,
            duration_ms,
        }
    }
}

impl From<lash_core::ToolIntentIdentity> for RemoteToolIntentIdentity {
    fn from(value: lash_core::ToolIntentIdentity) -> Self {
        Self {
            session_id: value.session_id,
            execution_scope_id: value.execution_scope_id,
            tool_call_id: value.tool_call_id,
            intent_index: value.intent_index,
            replay_key: value.replay_key,
        }
    }
}

impl From<lash_core::ToolIntentKind> for RemoteToolIntentKind {
    fn from(value: lash_core::ToolIntentKind) -> Self {
        match value {
            lash_core::ToolIntentKind::StartProcess => Self::StartProcess,
            lash_core::ToolIntentKind::SignalProcess => Self::SignalProcess,
            lash_core::ToolIntentKind::CancelProcess => Self::CancelProcess,
            lash_core::ToolIntentKind::EmitProcessEvent => Self::EmitProcessEvent,
            lash_core::ToolIntentKind::EmitTrigger => Self::EmitTrigger,
        }
    }
}

impl From<lash_core::ToolIntentRefusalReason> for RemoteToolIntentRefusalReason {
    fn from(value: lash_core::ToolIntentRefusalReason) -> Self {
        use lash_core::ToolIntentRefusalReason as Core;
        match value {
            Core::UnsupportedProtocolVersion { recorded } => {
                Self::UnsupportedProtocolVersion { recorded }
            }
            Core::MissingToolCallId => Self::MissingToolCallId,
            Core::IntentIndexOverflow => Self::IntentIndexOverflow,
            Core::CountBudgetExceeded { actual, maximum } => {
                Self::CountBudgetExceeded { actual, maximum }
            }
            Core::CanonicalByteBudgetExceeded { actual, maximum } => {
                Self::CanonicalByteBudgetExceeded { actual, maximum }
            }
            Core::PerKindBudgetExceeded {
                kind,
                actual,
                maximum,
            } => Self::PerKindBudgetExceeded {
                kind: kind.into(),
                actual,
                maximum,
            },
            Core::SessionMismatch { expected, recorded } => {
                Self::SessionMismatch { expected, recorded }
            }
            Core::CommandFailed { code, message } => Self::CommandFailed { code, message },
        }
    }
}

impl From<lash_core::ToolIntentExecutionOutcome> for RemoteToolIntentExecutionOutcome {
    fn from(value: lash_core::ToolIntentExecutionOutcome) -> Self {
        match value {
            lash_core::ToolIntentExecutionOutcome::Executed {
                identity,
                kind,
                result,
                parent_end,
            } => Self::Executed {
                identity: identity.into(),
                kind: kind.into(),
                result,
                parent_end: parent_end.map(Into::into),
            },
            lash_core::ToolIntentExecutionOutcome::Refused {
                identity,
                intent_index,
                kind,
                refusal,
            } => Self::Refused {
                identity: identity.map(Into::into),
                intent_index,
                kind: kind.into(),
                refusal: refusal.into(),
            },
            lash_core::ToolIntentExecutionOutcome::ProtocolRefused { refusal } => {
                Self::ProtocolRefused {
                    refusal: refusal.into(),
                }
            }
        }
    }
}

impl From<lash_core::ProcessParentEndPolicy> for RemoteProcessParentEndPolicy {
    fn from(value: lash_core::ProcessParentEndPolicy) -> Self {
        match value {
            lash_core::ProcessParentEndPolicy::Abandon => Self::Abandon,
            lash_core::ProcessParentEndPolicy::Cancel => Self::Cancel,
        }
    }
}

impl From<lash_core::ToolIntentParentEnd> for RemoteToolIntentParentEnd {
    fn from(value: lash_core::ToolIntentParentEnd) -> Self {
        Self {
            process_id: value.process_id,
            policy: value.policy.into(),
        }
    }
}

impl From<lash_core::ToolCallRecord> for RemoteToolCallRecord {
    fn from(value: lash_core::ToolCallRecord) -> Self {
        let lash_core::ToolCallRecord {
            call_id,
            tool,
            args,
            output,
            duration_ms,
        } = value;
        Self {
            call_id,
            tool_name: tool,
            args,
            outcome: output.into(),
            duration_ms,
        }
    }
}

impl From<lash_core::ToolCallOutput> for RemoteToolCallOutcome {
    fn from(value: lash_core::ToolCallOutput) -> Self {
        // `control` is a local turn-control signal and never crosses the wire.
        let lash_core::ToolCallOutput {
            outcome,
            control: _,
        } = value;
        match outcome {
            lash_core::ToolCallOutcome::Success(value) => Self::Success(value.to_json_value()),
            lash_core::ToolCallOutcome::Failure(value) => Self::Failure(value.to_json_value()),
            lash_core::ToolCallOutcome::Cancelled(value) => Self::Cancelled(value.to_json_value()),
        }
    }
}

impl From<lash_core::facade_support::TurnIssue> for RemoteTurnIssue {
    fn from(value: lash_core::facade_support::TurnIssue) -> Self {
        let lash_core::facade_support::TurnIssue {
            kind,
            code,
            terminal_reason,
            message,
            raw,
            retryable,
            provider_failure_kind,
        } = value;
        Self {
            kind,
            code,
            terminal_reason: terminal_reason.map(Into::into),
            message,
            raw,
            retryable,
            provider_failure_kind: provider_failure_kind.map(Into::into),
        }
    }
}
