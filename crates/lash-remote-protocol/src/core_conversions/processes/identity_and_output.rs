use super::*;

fn restore_frame_node_id(value: String) -> lash_core::FrameNodeId {
    lash_core::FrameNodeId::new(value)
        .expect("remote frame ids are validated before conversion to core")
}

impl From<lash_core::ProcessRef> for RemoteProcessRef {
    fn from(value: lash_core::ProcessRef) -> Self {
        Self {
            process_id: value.process_id,
            incarnation: value.incarnation.registration_sequence(),
        }
    }
}

impl From<RemoteProcessRef> for lash_core::ProcessRef {
    fn from(value: RemoteProcessRef) -> Self {
        Self::new(
            value.process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(value.incarnation),
        )
    }
}

impl From<lash_core::SessionScope> for RemoteSessionScope {
    fn from(value: lash_core::SessionScope) -> Self {
        let lash_core::SessionScope {
            session_id,
            agent_frame_id,
        } = value;
        Self {
            session_id,
            agent_frame_id: agent_frame_id.map(Into::into),
        }
    }
}

impl From<RemoteSessionScope> for lash_core::SessionScope {
    fn from(value: RemoteSessionScope) -> Self {
        let RemoteSessionScope {
            session_id,
            agent_frame_id,
        } = value;
        Self {
            session_id,
            agent_frame_id: agent_frame_id.map(restore_frame_node_id),
        }
    }
}

impl From<lash_core::ProcessOriginator> for RemoteProcessOriginator {
    fn from(value: lash_core::ProcessOriginator) -> Self {
        match value {
            lash_core::ProcessOriginator::Host { scope } => Self::Host { scope },
            lash_core::ProcessOriginator::Session {
                session_id,
                agent_frame_id,
            } => Self::Session {
                session_id,
                agent_frame_id: agent_frame_id.map(Into::into),
            },
        }
    }
}

impl From<RemoteProcessOriginator> for lash_core::ProcessOriginator {
    fn from(value: RemoteProcessOriginator) -> Self {
        match value {
            RemoteProcessOriginator::Host { scope } => Self::Host { scope },
            RemoteProcessOriginator::Session {
                session_id,
                agent_frame_id,
            } => Self::Session {
                session_id,
                agent_frame_id: agent_frame_id.map(restore_frame_node_id),
            },
        }
    }
}

impl From<lash_core::ProcessProvenance> for RemoteProcessProvenance {
    fn from(value: lash_core::ProcessProvenance) -> Self {
        let lash_core::ProcessProvenance {
            originator,
            caused_by,
        } = value;
        Self {
            originator: originator.into(),
            caused_by: caused_by.map(Into::into),
        }
    }
}

impl From<RemoteProcessProvenance> for lash_core::ProcessProvenance {
    fn from(value: RemoteProcessProvenance) -> Self {
        let RemoteProcessProvenance {
            originator,
            caused_by,
        } = value;
        Self {
            originator: originator.into(),
            caused_by: caused_by.map(Into::into),
        }
    }
}

impl From<serde_json::Value> for RemoteProcessDefinitionIdentity {
    fn from(value: serde_json::Value) -> Self {
        Self { value }
    }
}

impl From<RemoteProcessDefinitionIdentity> for serde_json::Value {
    fn from(value: RemoteProcessDefinitionIdentity) -> Self {
        value.value
    }
}

impl From<lash_core::ProcessIdentity> for RemoteProcessIdentity {
    fn from(value: lash_core::ProcessIdentity) -> Self {
        let lash_core::ProcessIdentity {
            kind,
            label,
            definition,
        } = value;
        Self {
            kind,
            label,
            definition: definition.map(Into::into),
        }
    }
}

impl From<RemoteProcessIdentity> for lash_core::ProcessIdentity {
    fn from(value: RemoteProcessIdentity) -> Self {
        let RemoteProcessIdentity {
            kind,
            label,
            definition,
        } = value;
        Self {
            kind,
            label,
            definition: definition.map(Into::into),
        }
    }
}

impl From<lash_core::ToolFailureClass> for RemoteToolFailureClass {
    fn from(value: lash_core::ToolFailureClass) -> Self {
        match value {
            lash_core::ToolFailureClass::InvalidRequest => Self::InvalidRequest,
            lash_core::ToolFailureClass::Io => Self::Io,
            lash_core::ToolFailureClass::Unavailable => Self::Unavailable,
            lash_core::ToolFailureClass::PermissionDenied => Self::PermissionDenied,
            lash_core::ToolFailureClass::Timeout => Self::Timeout,
            lash_core::ToolFailureClass::Execution => Self::Execution,
            lash_core::ToolFailureClass::External => Self::External,
            lash_core::ToolFailureClass::ResourceLimit => Self::ResourceLimit,
            lash_core::ToolFailureClass::Internal => Self::Internal,
        }
    }
}

impl From<RemoteToolFailureClass> for lash_core::ToolFailureClass {
    fn from(value: RemoteToolFailureClass) -> Self {
        match value {
            RemoteToolFailureClass::InvalidRequest => Self::InvalidRequest,
            RemoteToolFailureClass::Io => Self::Io,
            RemoteToolFailureClass::Unavailable => Self::Unavailable,
            RemoteToolFailureClass::PermissionDenied => Self::PermissionDenied,
            RemoteToolFailureClass::Timeout => Self::Timeout,
            RemoteToolFailureClass::Execution => Self::Execution,
            RemoteToolFailureClass::External => Self::External,
            RemoteToolFailureClass::ResourceLimit => Self::ResourceLimit,
            RemoteToolFailureClass::Internal => Self::Internal,
        }
    }
}

impl TryFrom<lash_core::ProcessAwaitOutput> for RemoteProcessAwaitOutput {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessAwaitOutput) -> Result<Self, Self::Error> {
        match value {
            lash_core::ProcessAwaitOutput::Settled { output } => Ok(Self::Settled {
                output: output.try_into()?,
            }),
            lash_core::ProcessAwaitOutput::Abandoned { evidence, control } => Ok(Self::Abandoned {
                evidence: (*evidence).try_into()?,
                control: control
                    .map(|control| {
                        encode_remote_json(control, "RemoteProcessAwaitOutput", "control")
                    })
                    .transpose()?,
            }),
            lash_core::ProcessAwaitOutput::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            } => Ok(Self::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }),
        }
    }
}

impl TryFrom<RemoteProcessAwaitOutput> for lash_core::ProcessAwaitOutput {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessAwaitOutput) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessAwaitOutput")?;
        match value {
            RemoteProcessAwaitOutput::Settled { output } => Ok(Self::Settled {
                output: output.try_into()?,
            }),
            RemoteProcessAwaitOutput::Abandoned { evidence, control } => Ok(Self::Abandoned {
                evidence: Box::new(evidence.try_into()?),
                control: decode_remote_tool_control(control, "RemoteProcessAwaitOutput")?,
            }),
            RemoteProcessAwaitOutput::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            } => Ok(Self::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }),
        }
    }
}

impl TryFrom<lash_core::ToolCallOutput> for RemoteProcessToolCallOutput {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ToolCallOutput) -> Result<Self, Self::Error> {
        let lash_core::ToolCallOutput { outcome, control } = value;
        let outcome = match outcome {
            lash_core::ToolCallOutcome::Success(value) => RemoteProcessToolCallOutcome::Success(
                encode_remote_json(value, "RemoteProcessAwaitOutput", "output.outcome.success")?,
            ),
            lash_core::ToolCallOutcome::Failure(failure) => {
                let lash_core::ToolFailure {
                    class,
                    code,
                    message,
                    source,
                    retry,
                    raw,
                } = failure;
                RemoteProcessToolCallOutcome::Failure(RemoteProcessToolFailure {
                    class: class.into(),
                    code,
                    message,
                    source: source.into(),
                    retry: retry.into(),
                    raw: raw
                        .map(|raw| {
                            encode_remote_json(
                                raw,
                                "RemoteProcessAwaitOutput",
                                "output.outcome.failure.raw",
                            )
                        })
                        .transpose()?,
                })
            }
            lash_core::ToolCallOutcome::Cancelled(cancellation) => {
                let lash_core::ToolCancellation {
                    message,
                    source,
                    raw,
                } = cancellation;
                RemoteProcessToolCallOutcome::Cancelled(RemoteProcessToolCancellation {
                    message,
                    source: source.into(),
                    raw: raw
                        .map(|raw| {
                            encode_remote_json(
                                raw,
                                "RemoteProcessAwaitOutput",
                                "output.outcome.cancelled.raw",
                            )
                        })
                        .transpose()?,
                })
            }
        };
        Ok(Self {
            outcome,
            control: control
                .map(|control| {
                    encode_remote_json(control, "RemoteProcessAwaitOutput", "output.control")
                })
                .transpose()?,
        })
    }
}

impl TryFrom<RemoteProcessToolCallOutput> for lash_core::ToolCallOutput {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessToolCallOutput) -> Result<Self, Self::Error> {
        let RemoteProcessToolCallOutput { outcome, control } = value;
        let outcome = match outcome {
            RemoteProcessToolCallOutcome::Success(value) => lash_core::ToolCallOutcome::Success(
                decode_remote_json(value, "RemoteProcessAwaitOutput", "output.outcome.success")?,
            ),
            RemoteProcessToolCallOutcome::Failure(failure) => {
                let RemoteProcessToolFailure {
                    class,
                    code,
                    message,
                    source,
                    retry,
                    raw,
                } = failure;
                lash_core::ToolCallOutcome::Failure(lash_core::ToolFailure {
                    class: class.into(),
                    code,
                    message,
                    source: source.into(),
                    retry: retry.into(),
                    raw: raw
                        .map(|raw| {
                            decode_remote_json(
                                raw,
                                "RemoteProcessAwaitOutput",
                                "output.outcome.failure.raw",
                            )
                        })
                        .transpose()?,
                })
            }
            RemoteProcessToolCallOutcome::Cancelled(cancellation) => {
                let RemoteProcessToolCancellation {
                    message,
                    source,
                    raw,
                } = cancellation;
                lash_core::ToolCallOutcome::Cancelled(lash_core::ToolCancellation {
                    message,
                    source: source.into(),
                    raw: raw
                        .map(|raw| {
                            decode_remote_json(
                                raw,
                                "RemoteProcessAwaitOutput",
                                "output.outcome.cancelled.raw",
                            )
                        })
                        .transpose()?,
                })
            }
        };
        Ok(Self {
            outcome,
            control: decode_remote_tool_control(control, "RemoteProcessAwaitOutput")?,
        })
    }
}

impl From<lash_core::ToolFailureSource> for RemoteProcessToolFailureSource {
    fn from(value: lash_core::ToolFailureSource) -> Self {
        match value {
            lash_core::ToolFailureSource::Runtime => Self::Runtime,
            lash_core::ToolFailureSource::Tool => Self::Tool,
            lash_core::ToolFailureSource::Plugin => Self::Plugin,
            lash_core::ToolFailureSource::Policy => Self::Policy,
            lash_core::ToolFailureSource::Cancellation => Self::Cancellation,
            lash_core::ToolFailureSource::UnknownLegacy => Self::UnknownLegacy,
        }
    }
}

impl From<RemoteProcessToolFailureSource> for lash_core::ToolFailureSource {
    fn from(value: RemoteProcessToolFailureSource) -> Self {
        match value {
            RemoteProcessToolFailureSource::Runtime => Self::Runtime,
            RemoteProcessToolFailureSource::Tool => Self::Tool,
            RemoteProcessToolFailureSource::Plugin => Self::Plugin,
            RemoteProcessToolFailureSource::Policy => Self::Policy,
            RemoteProcessToolFailureSource::Cancellation => Self::Cancellation,
            RemoteProcessToolFailureSource::UnknownLegacy => Self::UnknownLegacy,
        }
    }
}

impl From<lash_core::ToolRetryStatus> for RemoteProcessToolRetryStatus {
    fn from(value: lash_core::ToolRetryStatus) -> Self {
        match value {
            lash_core::ToolRetryStatus::Never => Self::Never,
            lash_core::ToolRetryStatus::Safe { after_ms } => Self::Safe { after_ms },
            lash_core::ToolRetryStatus::Exhausted { attempts } => Self::Exhausted { attempts },
            lash_core::ToolRetryStatus::UnknownLegacy => Self::UnknownLegacy,
        }
    }
}

impl From<RemoteProcessToolRetryStatus> for lash_core::ToolRetryStatus {
    fn from(value: RemoteProcessToolRetryStatus) -> Self {
        match value {
            RemoteProcessToolRetryStatus::Never => Self::Never,
            RemoteProcessToolRetryStatus::Safe { after_ms } => Self::Safe { after_ms },
            RemoteProcessToolRetryStatus::Exhausted { attempts } => Self::Exhausted { attempts },
            RemoteProcessToolRetryStatus::UnknownLegacy => Self::UnknownLegacy,
        }
    }
}

fn decode_remote_tool_control(
    value: Option<serde_json::Value>,
    type_name: &'static str,
) -> Result<Option<lash_core::ToolControl>, RemoteProtocolError> {
    value
        .map(|value| decode_remote_json(value, type_name, "control"))
        .transpose()
}
