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
            lash_core::ProcessStatus::CallerDeparted => Self::CallerDeparted,
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
            RemoteProcessStatus::CallerDeparted => Self::CallerDeparted,
        }
    }
}

impl From<lash_core::ProcessExternalRef> for RemoteProcessExternalRef {
    fn from(value: lash_core::ProcessExternalRef) -> Self {
        let lash_core::ProcessExternalRef {
            backend,
            id,
            metadata,
        } = value;
        Self {
            backend,
            id,
            metadata,
        }
    }
}

impl From<RemoteProcessExternalRef> for lash_core::ProcessExternalRef {
    fn from(value: RemoteProcessExternalRef) -> Self {
        let RemoteProcessExternalRef {
            backend,
            id,
            metadata,
        } = value;
        Self {
            backend,
            id,
            metadata,
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
            lash_core::ProcessInput::ToolCall { call } => Ok(Self::ToolCall {
                prepared_tool_call: serde_json::to_value(call).map_err(|err| {
                    RemoteProtocolError::InvalidEnvelope {
                        type_name: "RemoteProcessInput",
                        message: format!("invalid prepared tool call: {err}"),
                    }
                })?,
            }),
            lash_core::ProcessInput::Engine { kind, payload } => Ok(Self::Engine { kind, payload }),
            lash_core::ProcessInput::SessionTurn {
                definition_key,
                create_request,
                turn_input,
                output_contract,
            } => Ok(Self::SessionTurn {
                definition_key,
                create_request: serde_json::to_value(create_request.as_ref()).map_err(|err| {
                    RemoteProtocolError::InvalidEnvelope {
                        type_name: "RemoteProcessInput",
                        message: format!("invalid session create request: {err}"),
                    }
                })?,
                turn_input: RemoteTurnInput::try_from(*turn_input)?,
                output_contract: output_contract.into(),
            }),
            lash_core::ProcessInput::External { metadata } => Ok(Self::External { metadata }),
        }
    }
}

impl TryFrom<RemoteProcessInput> for lash_core::ProcessInput {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessInput) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessInput")?;
        match value {
            RemoteProcessInput::ToolCall { prepared_tool_call } => Ok(Self::ToolCall {
                call: decode_remote_json(
                    prepared_tool_call,
                    "RemoteProcessInput",
                    "prepared_tool_call",
                )?,
            }),
            RemoteProcessInput::Engine { kind, payload } => Ok(Self::Engine { kind, payload }),
            RemoteProcessInput::SessionTurn {
                definition_key,
                create_request,
                turn_input,
                output_contract,
            } => Ok(Self::SessionTurn {
                definition_key,
                create_request: Box::new(decode_remote_json(
                    create_request,
                    "RemoteProcessInput",
                    "create_request",
                )?),
                turn_input: Box::new(lash_core::TurnInput::try_from(turn_input)?),
                output_contract: output_contract.into(),
            }),
            RemoteProcessInput::External { metadata } => Ok(Self::External { metadata }),
        }
    }
}
