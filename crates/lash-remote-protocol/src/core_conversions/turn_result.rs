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
            // Retained outputs are what the turn's commit names; the remote
            // result keeps its existing shape.
            retained_outputs: _,
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
                // Observation outputs project frame switches without seed bodies.
                Self::AgentFrameSwitch {
                    frame_key: frame_key.as_str().to_string(),
                    task,
                }
            }
            lash_core::facade_support::TurnOutcome::SegmentBoundary { reason } => {
                Self::SegmentBoundary {
                    reason: match reason {
                        lash_core::BoundaryReason::JournalBudget => {
                            RemoteBoundaryReason::JournalBudget
                        }
                        lash_core::BoundaryReason::HandOver => RemoteBoundaryReason::HandOver,
                    },
                }
            }
            lash_core::facade_support::TurnOutcome::Stopped(stop) => {
                Self::Stopped { stop: stop.into() }
            }
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
            owner: value.owner,
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
            Core::IntentIndexOverflow => Self::IntentIndexOverflow,
            Core::ExecutionEnvMissing => Self::ExecutionEnvMissing,
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
            Core::OwnerMismatch { expected, recorded } => {
                Self::OwnerMismatch { expected, recorded }
            }
            Core::ForeignTriggerOwnerScope { expected, recorded } => {
                Self::ForeignTriggerOwnerScope {
                    expected: expected.into(),
                    recorded: recorded.into(),
                }
            }
            Core::ForeignTriggerActor { expected, recorded } => Self::ForeignTriggerActor {
                expected: expected.into(),
                recorded: recorded.into(),
            },
            Core::CommandFailed { cause } => Self::CommandFailed { cause },
            Core::MintingGroupChildCancelled => Self::MintingGroupChildCancelled,
            Core::DeclaredStartIdentityMismatch { expected, recorded } => {
                Self::DeclaredStartIdentityMismatch {
                    expected: Box::new((*expected).into()),
                    recorded: Box::new((*recorded).into()),
                }
            }
        }
    }
}

impl TryFrom<lash_core::ToolIntentExecutionOutcome> for RemoteToolIntentExecutionOutcome {
    type Error = crate::RemoteProtocolError;
    fn try_from(value: lash_core::ToolIntentExecutionOutcome) -> Result<Self, Self::Error> {
        Ok(match value {
            lash_core::ToolIntentExecutionOutcome::Executed { identity, realized } => {
                Self::Executed {
                    identity: identity.into(),
                    realized: realized.try_into()?,
                }
            }
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
        })
    }
}

impl From<lash_core::ToolCallRecord> for RemoteToolCallRecord {
    fn from(value: lash_core::ToolCallRecord) -> Self {
        let lash_core::ToolCallRecord {
            call_id,
            provider_call_id,
            tool,
            args,
            output,
        } = value;
        Self {
            call_id,
            provider_call_id,
            tool_name: tool,
            args,
            output: output.into(),
        }
    }
}

impl From<lash_core::ToolCallOutput> for RemoteToolCallOutput {
    fn from(value: lash_core::ToolCallOutput) -> Self {
        let lash_core::ToolCallOutput {
            outcome,
            control,
            view,
            projection_value,
        } = value;
        Self {
            outcome: outcome.into(),
            control: control.map(Into::into),
            view,
            projection_value,
        }
    }
}

impl From<lash_core::ToolCallOutcome> for RemoteToolCallOutcome {
    fn from(value: lash_core::ToolCallOutcome) -> Self {
        match value {
            lash_core::ToolCallOutcome::Success(value) => Self::Success(value.to_json_value()),
            lash_core::ToolCallOutcome::Failure(failure) => Self::Failure(failure.into()),
            lash_core::ToolCallOutcome::Cancelled(cancellation) => {
                Self::Cancelled(cancellation.into())
            }
        }
    }
}

impl From<lash_core::ToolFailure> for RemoteToolFailure {
    fn from(value: lash_core::ToolFailure) -> Self {
        let lash_core::ToolFailure {
            class,
            code,
            message,
            source,
            retry,
            cause,
            raw,
        } = value;
        Self {
            class: class.into(),
            code,
            message,
            source,
            retry,
            cause,
            raw: raw.map(|value| value.to_json_value()),
        }
    }
}

impl From<lash_core::ToolCancellation> for RemoteToolCancellation {
    fn from(value: lash_core::ToolCancellation) -> Self {
        let lash_core::ToolCancellation {
            message,
            source,
            origin,
            raw,
        } = value;
        Self {
            message,
            source,
            origin,
            raw: raw.map(|value| value.to_json_value()),
        }
    }
}

impl From<lash_core::ToolControl> for RemoteToolControlProjection {
    fn from(value: lash_core::ToolControl) -> Self {
        match value {
            lash_core::ToolControl::SwitchAgentFrame {
                frame_key,
                initial_nodes,
                task,
            } => Self::SwitchAgentFrame {
                frame_key,
                seed_count: initial_nodes.len(),
                task,
            },
            lash_core::ToolControl::Finish { value } => Self::Finish {
                value: value.to_json_value(),
            },
            lash_core::ToolControl::Fail { failure } => Self::Fail {
                failure: failure.into(),
            },
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
            plugin_failures,
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
            plugin_failures,
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

impl From<&lash_core::store::ParkReason> for RemoteTurnParkReason {
    fn from(value: &lash_core::store::ParkReason) -> Self {
        Self {
            code: value.code().as_str().to_string(),
            message: value.message().to_string(),
            profile_key: value.profile_key().map(|key| key.as_str().to_string()),
        }
    }
}

macro_rules! convert_realized_payload {
    (CancelProcess, $result:expr) => {
        Box::new($result.into())
    };
    (GetDefinition, $result:expr) => {
        (*$result).into()
    };
    (PublishDefinition, $result:expr) => {
        (*$result).into()
    };
    (SignalProcess, $result:expr) => {
        Box::new((*$result).try_into()?)
    };
    (EmitProcessEvent, $result:expr) => {
        Box::new((*$result).try_into()?)
    };
    (RegisterTrigger, $result:expr) => {
        Box::new((*$result).try_into()?)
    };
    ($variant:ident, $result:expr) => {
        $result.into()
    };
}
macro_rules! define_realized_conversion {
    ($($variant:ident $wire:literal,)*) => {
        impl TryFrom<lash_core::ToolIntentRealized> for RemoteToolIntentRealized {
            type Error = crate::RemoteProtocolError;
            fn try_from(value: lash_core::ToolIntentRealized) -> Result<Self, Self::Error> {
                Ok(match value {
                    $(lash_core::ToolIntentRealized::$variant(result) => Self::$variant(convert_realized_payload!($variant, result)),)*
                })
            }
        }
    };
}
lash_sansio::tool_intent_variants!(define_realized_conversion);

impl TryFrom<lash_core::TriggerMutationReceipt> for crate::RemoteTriggerMutationReceipt {
    type Error = crate::RemoteProtocolError;
    fn try_from(value: lash_core::TriggerMutationReceipt) -> Result<Self, Self::Error> {
        let lash_core::TriggerMutationReceipt {
            owner_scope,
            subscription_key,
            subscription_id,
            incarnation,
            revision,
            definition_fingerprint,
            enabled,
            disposition,
            record_snapshot,
        } = value;
        Ok(Self {
            owner_scope: owner_scope.into(),
            subscription_key,
            subscription_id,
            incarnation,
            revision,
            definition_fingerprint,
            enabled,
            disposition,
            record_snapshot: record_snapshot.try_into()?,
        })
    }
}
