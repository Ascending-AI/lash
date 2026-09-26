use super::*;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

impl RemoteTurnReport {
    pub fn from_core(
        session_id: impl Into<SessionId>,
        turn_id: impl Into<TurnId>,
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
            llm_calls,
            tool_calls,
            // Omission accounting is an internal bounded-stream surface; the
            // remote protocol keeps its existing result shape.
            omitted: _,
            // Failure evidence is a durable turn-receipt read surface, not a
            // duplicate remote execution-result payload.
            failure_evidence: _,
            errors,
        } = turn;
        let activities = activities.into_iter().collect::<Vec<_>>();
        let outcome = RemoteTurnOutcome::from(outcome);
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            outcome,
            assistant_output: assistant_output.into(),
            usage: RemoteTurnUsageReport {
                usage: RemoteUsage::from(token_usage),
            },
            execution: execution.into(),
            tool_calls: tool_calls
                .into_iter()
                .map(RemoteToolCallRecord::from)
                .collect(),
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
                // See `observations::encode_remote_tool_call_output` for the projection boundary.
                Self::AgentFrameSwitch {
                    frame_key: frame_key.as_str().to_string(),
                    task,
                }
            }
            lash_core::facade_support::TurnOutcome::Stopped(stop) => {
                Self::Stopped { stop: stop.into() }
            }
            lash_core::facade_support::TurnOutcome::Queued { ahead } => Self::Queued { ahead },
        }
    }
}

impl From<lash_core::facade_support::TurnFinish> for RemoteTurnFinish {
    fn from(value: lash_core::facade_support::TurnFinish) -> Self {
        match value {
            lash_core::facade_support::TurnFinish::AssistantMessage { text } => {
                Self::AssistantMessage { text }
            }
            lash_core::facade_support::TurnFinish::FinalValue { value } => {
                Self::FinalValue { value }
            }
            lash_core::facade_support::TurnFinish::ToolValue { tool_name, value } => {
                Self::ToolValue { tool_name, value }
            }
        }
    }
}

impl From<lash_core::facade_support::TurnStop> for RemoteTurnStop {
    fn from(value: lash_core::facade_support::TurnStop) -> Self {
        match value {
            lash_core::facade_support::TurnStop::Cancelled { evidence } => Self::Cancelled {
                evidence: evidence.into(),
            },
            lash_core::facade_support::TurnStop::Incomplete => Self::Incomplete,
            lash_core::facade_support::TurnStop::InvalidInput => Self::InvalidInput,
            lash_core::facade_support::TurnStop::MaxTurns => Self::MaxTurns,
            lash_core::facade_support::TurnStop::ToolFailure => Self::ToolFailure,
            lash_core::facade_support::TurnStop::ProviderError => Self::ProviderError,
            lash_core::facade_support::TurnStop::ContextOverflow => Self::ContextOverflow,
            lash_core::facade_support::TurnStop::PluginAbort => Self::PluginAbort,
            lash_core::facade_support::TurnStop::RuntimeError => Self::RuntimeError,
            lash_core::facade_support::TurnStop::SubmittedError { value } => {
                Self::SubmittedError { value }
            }
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
            minting_emission_replay_key: value.minting_emission_replay_key,
        }
    }
}

macro_rules! define_remote_tool_intent_kind_conversions {
    ($($variant:ident $wire:literal,)*) => {
        impl From<lash_core::ToolIntentKind> for RemoteToolIntentKind {
            fn from(value: lash_core::ToolIntentKind) -> Self {
                match value {
                    $(lash_core::ToolIntentKind::$variant => Self::$variant,)*
                }
            }
        }

        impl From<RemoteToolIntentKind> for lash_core::ToolIntentKind {
            fn from(value: RemoteToolIntentKind) -> Self {
                match value {
                    $(RemoteToolIntentKind::$variant => Self::$variant,)*
                }
            }
        }
    };
}

lash_sansio::tool_intent_variants!(define_remote_tool_intent_kind_conversions);

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
            Core::MintingGroupChildCancelled => Self::MintingGroupChildCancelled,
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
            } => Self::Executed {
                identity: identity.into(),
                kind: kind.into(),
                result,
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

impl From<lash_core::ToolCallRecord> for RemoteToolCallRecord {
    fn from(value: lash_core::ToolCallRecord) -> Self {
        let lash_core::ToolCallRecord {
            call_id,
            tool,
            args,
            output,
        } = value;
        Self {
            call_id,
            tool_name: tool,
            args,
            outcome: output.into(),
        }
    }
}

impl From<lash_core::ToolCallOutput> for RemoteToolCallOutcome {
    fn from(value: lash_core::ToolCallOutput) -> Self {
        // See `observations::encode_remote_tool_call_output` for the projection boundary.
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
            severity,
            kind,
            code,
            terminal_reason,
            message,
            raw,
            retryable,
            provider_failure_kind,
        } = value;
        Self {
            severity: severity.into(),
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

impl From<lash_core::facade_support::TurnIssueSeverity> for RemoteTurnIssueSeverity {
    fn from(value: lash_core::facade_support::TurnIssueSeverity) -> Self {
        match value {
            lash_core::facade_support::TurnIssueSeverity::Advisory => Self::Advisory,
            lash_core::facade_support::TurnIssueSeverity::Blocking => Self::Blocking,
        }
    }
}
