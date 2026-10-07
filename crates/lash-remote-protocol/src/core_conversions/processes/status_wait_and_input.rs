use super::*;

impl From<lash_core::ProcessStatus> for RemoteProcessStatus {
    fn from(value: lash_core::ProcessStatus) -> Self {
        match value {
            lash_core::ProcessStatus::Running => Self::Running,
            lash_core::ProcessStatus::Waiting => Self::Waiting,
            lash_core::ProcessStatus::Completed => Self::Completed,
            lash_core::ProcessStatus::Failed => Self::Failed,
            lash_core::ProcessStatus::Cancelled => Self::Cancelled,
            lash_core::ProcessStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl From<RemoteProcessStatus> for lash_core::ProcessStatus {
    fn from(value: RemoteProcessStatus) -> Self {
        match value {
            RemoteProcessStatus::Running => Self::Running,
            RemoteProcessStatus::Waiting => Self::Waiting,
            RemoteProcessStatus::Completed => Self::Completed,
            RemoteProcessStatus::Failed => Self::Failed,
            RemoteProcessStatus::Cancelled => Self::Cancelled,
            RemoteProcessStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl From<lash_core::TerminalProcessStatus> for RemoteTerminalProcessStatus {
    fn from(value: lash_core::TerminalProcessStatus) -> Self {
        match value {
            lash_core::TerminalProcessStatus::Completed => Self::Completed,
            lash_core::TerminalProcessStatus::Failed => Self::Failed,
            lash_core::TerminalProcessStatus::Cancelled => Self::Cancelled,
            lash_core::TerminalProcessStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl From<RemoteTerminalProcessStatus> for lash_core::TerminalProcessStatus {
    fn from(value: RemoteTerminalProcessStatus) -> Self {
        match value {
            RemoteTerminalProcessStatus::Completed => Self::Completed,
            RemoteTerminalProcessStatus::Failed => Self::Failed,
            RemoteTerminalProcessStatus::Cancelled => Self::Cancelled,
            RemoteTerminalProcessStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl TryFrom<lash_core::ProcessTerminal> for RemoteProcessTerminal {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessTerminal) -> Result<Self, Self::Error> {
        RemoteProcessAwaitOutput::try_from(lash_core::ProcessAwaitOutput::from(value))?.try_into()
    }
}

impl TryFrom<RemoteProcessTerminal> for lash_core::ProcessTerminal {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessTerminal) -> Result<Self, Self::Error> {
        let output =
            lash_core::ProcessAwaitOutput::try_from(RemoteProcessAwaitOutput::from(value))?;
        Self::try_from(output).map_err(|error| RemoteProtocolError::InvalidEnvelope {
            type_name: "RemoteProcessTerminal",
            message: error.to_string(),
        })
    }
}

impl TryFrom<lash_core::ProcessLifecycleState> for RemoteProcessLifecycleState {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessLifecycleState) -> Result<Self, Self::Error> {
        Ok(match value {
            lash_core::ProcessLifecycleState::Running {} => Self::Running {},
            lash_core::ProcessLifecycleState::Waiting { wait } => {
                Self::Waiting { wait: wait.into() }
            }
            lash_core::ProcessLifecycleState::Terminal { outcome } => Self::Terminal {
                outcome: outcome.try_into()?,
            },
        })
    }
}

impl TryFrom<RemoteProcessLifecycleState> for lash_core::ProcessLifecycleState {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessLifecycleState) -> Result<Self, Self::Error> {
        Ok(match value {
            RemoteProcessLifecycleState::Running {} => Self::Running {},
            RemoteProcessLifecycleState::Waiting { wait } => Self::Waiting { wait: wait.into() },
            RemoteProcessLifecycleState::Terminal { outcome } => Self::Terminal {
                outcome: outcome.try_into()?,
            },
        })
    }
}

impl From<lash_core::RetiredProcessStatus> for RemoteRetiredProcessStatus {
    fn from(value: lash_core::RetiredProcessStatus) -> Self {
        match value {
            lash_core::RetiredProcessStatus::Completed => Self::Completed,
            lash_core::RetiredProcessStatus::Failed => Self::Failed,
            lash_core::RetiredProcessStatus::Cancelled => Self::Cancelled,
            lash_core::RetiredProcessStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl From<RemoteRetiredProcessStatus> for lash_core::RetiredProcessStatus {
    fn from(value: RemoteRetiredProcessStatus) -> Self {
        match value {
            RemoteRetiredProcessStatus::Completed => Self::Completed,
            RemoteRetiredProcessStatus::Failed => Self::Failed,
            RemoteRetiredProcessStatus::Cancelled => Self::Cancelled,
            RemoteRetiredProcessStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl From<lash_core::ProcessExternalRef> for RemoteProcessExternalRef {
    fn from(value: lash_core::ProcessExternalRef) -> Self {
        let lash_core::ProcessExternalRef {
            backend,
            id,
            metadata,
            segment_ordinal,
        } = value;
        Self {
            backend,
            id,
            metadata,
            segment_ordinal,
        }
    }
}

impl From<RemoteProcessExternalRef> for lash_core::ProcessExternalRef {
    fn from(value: RemoteProcessExternalRef) -> Self {
        let RemoteProcessExternalRef {
            backend,
            id,
            metadata,
            segment_ordinal,
        } = value;
        Self {
            backend,
            id,
            metadata,
            segment_ordinal,
        }
    }
}

impl From<lash_core::WaitState> for RemoteProcessWaitState {
    fn from(value: lash_core::WaitState) -> Self {
        let lash_core::WaitState { kind, since_ms } = value;
        Self {
            kind: kind.into(),
            since_ms,
        }
    }
}

impl From<RemoteProcessWaitState> for lash_core::WaitState {
    fn from(value: RemoteProcessWaitState) -> Self {
        let RemoteProcessWaitState { kind, since_ms } = value;
        Self {
            kind: kind.into(),
            since_ms,
        }
    }
}

impl From<lash_core::WaitKind> for RemoteProcessWaitKind {
    fn from(value: lash_core::WaitKind) -> Self {
        match value {
            lash_core::WaitKind::Signal {
                name,
                event_type,
                key,
                ordinal,
            } => Self::Signal {
                name,
                event_type,
                key,
                ordinal,
            },
        }
    }
}

impl From<RemoteProcessWaitKind> for lash_core::WaitKind {
    fn from(value: RemoteProcessWaitKind) -> Self {
        match value {
            RemoteProcessWaitKind::Signal {
                name,
                event_type,
                key,
                ordinal,
            } => Self::Signal {
                name,
                event_type,
                key,
                ordinal,
            },
        }
    }
}

impl TryFrom<lash_core::ProcessInput> for RemoteProcessInput {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessInput) -> Result<Self, Self::Error> {
        match value {
            lash_core::ProcessInput::Engine { kind, payload } => Ok(Self::Engine { kind, payload }),
            lash_core::ProcessInput::SessionTurn {
                definition_key,
                create_request,
                turn_input,
                result,
            } => Ok(Self::SessionTurn {
                definition_key,
                create_request: serde_json::to_value(create_request.as_ref()).map_err(|err| {
                    RemoteProtocolError::InvalidEnvelope {
                        type_name: "RemoteProcessInput",
                        message: format!("invalid session create request: {err}"),
                    }
                })?,
                turn_input: RemoteTurnInput::try_from(*turn_input)?,
                result: result.into(),
            }),
        }
    }
}

impl TryFrom<lash_core::ProcessStartTarget> for RemoteProcessStartTarget {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessStartTarget) -> Result<Self, Self::Error> {
        match value {
            lash_core::ProcessStartTarget::Input(input) => Ok(Self::Input(input.try_into()?)),
            lash_core::ProcessStartTarget::Definition {
                definition_id,
                args,
                signature_claim,
            } => Ok(Self::Definition {
                definition_id,
                args,
                signature_claim: signature_claim.map(Into::into),
            }),
        }
    }
}

impl TryFrom<RemoteProcessStartTarget> for lash_core::ProcessStartTarget {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessStartTarget) -> Result<Self, Self::Error> {
        match value {
            RemoteProcessStartTarget::Input(input) => Ok(Self::Input(input.try_into()?)),
            RemoteProcessStartTarget::Definition {
                definition_id,
                args,
                signature_claim,
            } => Ok(Self::Definition {
                definition_id,
                args,
                signature_claim: signature_claim.map(Into::into),
            }),
        }
    }
}

impl TryFrom<RemoteProcessInput> for lash_core::ProcessInput {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessInput) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessInput")?;
        match value {
            RemoteProcessInput::Engine { kind, payload } => Ok(Self::Engine { kind, payload }),
            RemoteProcessInput::SessionTurn {
                definition_key,
                create_request,
                turn_input,
                result,
            } => Ok(Self::SessionTurn {
                definition_key,
                create_request: Box::new(decode_remote_json(
                    create_request,
                    "RemoteProcessInput",
                    "create_request",
                )?),
                turn_input: Box::new(lash_core::TurnInput::try_from(turn_input)?),
                result: result.into(),
            }),
        }
    }
}

impl From<lash_core::SessionTurnOutcome> for crate::RemoteSessionTurnOutcome {
    fn from(value: lash_core::SessionTurnOutcome) -> Self {
        match value {
            lash_core::SessionTurnOutcome::Turn => Self::Turn,
            lash_core::SessionTurnOutcome::FinalValue { schema } => Self::FinalValue { schema },
        }
    }
}

impl From<crate::RemoteSessionTurnOutcome> for lash_core::SessionTurnOutcome {
    fn from(value: crate::RemoteSessionTurnOutcome) -> Self {
        match value {
            crate::RemoteSessionTurnOutcome::Turn => Self::Turn,
            crate::RemoteSessionTurnOutcome::FinalValue { schema } => Self::FinalValue { schema },
        }
    }
}
