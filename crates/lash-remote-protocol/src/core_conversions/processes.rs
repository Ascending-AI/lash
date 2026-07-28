impl From<lash_core::SessionScope> for RemoteSessionScope {
    fn from(value: lash_core::SessionScope) -> Self {
        let lash_core::SessionScope {
            session_id,
            agent_frame_id,
        } = value;
        Self {
            session_id,
            agent_frame_id,
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
            agent_frame_id,
        }
    }
}

impl From<lash_core::ProcessOriginator> for RemoteProcessOriginator {
    fn from(value: lash_core::ProcessOriginator) -> Self {
        match value {
            lash_core::ProcessOriginator::Host { scope } => Self::Host { scope },
            lash_core::ProcessOriginator::Session { scope } => Self::Session {
                scope: scope.into(),
            },
        }
    }
}

impl From<RemoteProcessOriginator> for lash_core::ProcessOriginator {
    fn from(value: RemoteProcessOriginator) -> Self {
        match value {
            RemoteProcessOriginator::Host { scope } => Self::Host { scope },
            RemoteProcessOriginator::Session { scope } => Self::Session {
                scope: scope.into(),
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

impl From<lash_core::ProcessHandleDescriptor> for RemoteProcessHandleDescriptor {
    fn from(value: lash_core::ProcessHandleDescriptor) -> Self {
        let lash_core::ProcessHandleDescriptor { kind, label } = value;
        Self { kind, label }
    }
}

impl From<RemoteProcessHandleDescriptor> for lash_core::ProcessHandleDescriptor {
    fn from(value: RemoteProcessHandleDescriptor) -> Self {
        let RemoteProcessHandleDescriptor { kind, label } = value;
        Self { kind, label }
    }
}

impl From<lash_core::ProcessStartGrant> for RemoteProcessStartGrant {
    fn from(value: lash_core::ProcessStartGrant) -> Self {
        let lash_core::ProcessStartGrant {
            session_scope,
            descriptor,
        } = value;
        Self {
            session_scope: session_scope.into(),
            descriptor: descriptor.into(),
        }
    }
}

impl From<RemoteProcessStartGrant> for lash_core::ProcessStartGrant {
    fn from(value: RemoteProcessStartGrant) -> Self {
        let RemoteProcessStartGrant {
            session_scope,
            descriptor,
        } = value;
        Self {
            session_scope: session_scope.into(),
            descriptor: descriptor.into(),
        }
    }
}

impl From<lash_core::ProcessLifecycleStatus> for RemoteProcessLifecycleStatus {
    fn from(value: lash_core::ProcessLifecycleStatus) -> Self {
        match value {
            lash_core::ProcessLifecycleStatus::Running => Self::Running,
            lash_core::ProcessLifecycleStatus::Completed => Self::Completed,
            lash_core::ProcessLifecycleStatus::Failed => Self::Failed,
            lash_core::ProcessLifecycleStatus::Cancelled => Self::Cancelled,
            lash_core::ProcessLifecycleStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl From<RemoteProcessLifecycleStatus> for lash_core::ProcessLifecycleStatus {
    fn from(value: RemoteProcessLifecycleStatus) -> Self {
        match value {
            RemoteProcessLifecycleStatus::Running => Self::Running,
            RemoteProcessLifecycleStatus::Completed => Self::Completed,
            RemoteProcessLifecycleStatus::Failed => Self::Failed,
            RemoteProcessLifecycleStatus::Cancelled => Self::Cancelled,
            RemoteProcessLifecycleStatus::Abandoned => Self::Abandoned,
        }
    }
}

impl From<lash_core::ToolFailureClass> for RemoteToolFailureClass {
    fn from(value: lash_core::ToolFailureClass) -> Self {
        match value {
            lash_core::ToolFailureClass::InvalidRequest => Self::InvalidRequest,
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

impl From<lash_core::ProcessAwaitOutput> for RemoteProcessAwaitOutput {
    fn from(value: lash_core::ProcessAwaitOutput) -> Self {
        match value {
            lash_core::ProcessAwaitOutput::Success { value, control } => Self::Success {
                value,
                control: control.map(|control| {
                    serde_json::to_value(control).expect("tool control serializes")
                }),
            },
            lash_core::ProcessAwaitOutput::Failure {
                class,
                code,
                message,
                raw,
                control,
            } => Self::Failure {
                class: class.into(),
                code,
                message,
                raw,
                control: control.map(|control| {
                    serde_json::to_value(control).expect("tool control serializes")
                }),
            },
            lash_core::ProcessAwaitOutput::Cancelled {
                message,
                raw,
                control,
            } => Self::Cancelled {
                message,
                raw,
                control: control.map(|control| {
                    serde_json::to_value(control).expect("tool control serializes")
                }),
            },
            lash_core::ProcessAwaitOutput::Abandoned { evidence, control } => Self::Abandoned {
                evidence: (*evidence).into(),
                control: control.map(|control| {
                    serde_json::to_value(control).expect("tool control serializes")
                }),
            },
        }
    }
}

impl TryFrom<RemoteProcessAwaitOutput> for lash_core::ProcessAwaitOutput {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessAwaitOutput) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessAwaitOutput")?;
        match value {
            RemoteProcessAwaitOutput::Success { value, control } => Ok(Self::Success {
                value,
                control: decode_remote_tool_control(control, "RemoteProcessAwaitOutput")?,
            }),
            RemoteProcessAwaitOutput::Failure {
                class,
                code,
                message,
                raw,
                control,
            } => Ok(Self::Failure {
                class: class.into(),
                code,
                message,
                raw,
                control: decode_remote_tool_control(control, "RemoteProcessAwaitOutput")?,
            }),
            RemoteProcessAwaitOutput::Cancelled {
                message,
                raw,
                control,
            } => Ok(Self::Cancelled {
                message,
                raw,
                control: decode_remote_tool_control(control, "RemoteProcessAwaitOutput")?,
            }),
            RemoteProcessAwaitOutput::Abandoned { evidence, control } => Ok(Self::Abandoned {
                evidence: Box::new(evidence.try_into()?),
                control: decode_remote_tool_control(control, "RemoteProcessAwaitOutput")?,
            }),
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

impl From<lash_core::ProcessStatus> for RemoteProcessStatus {
    fn from(value: lash_core::ProcessStatus) -> Self {
        match value {
            lash_core::ProcessStatus::Running => Self::Running,
            lash_core::ProcessStatus::Completed { await_output } => Self::Completed {
                await_output: await_output.into(),
            },
            lash_core::ProcessStatus::Failed { await_output } => Self::Failed {
                await_output: await_output.into(),
            },
            lash_core::ProcessStatus::Cancelled { await_output } => Self::Cancelled {
                await_output: await_output.into(),
            },
            lash_core::ProcessStatus::Abandoned { await_output } => Self::Abandoned {
                await_output: await_output.into(),
            },
        }
    }
}

impl TryFrom<RemoteProcessStatus> for lash_core::ProcessStatus {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessStatus) -> Result<Self, Self::Error> {
        match value {
            RemoteProcessStatus::Running => Ok(Self::Running),
            RemoteProcessStatus::Completed { await_output } => Ok(Self::Completed {
                await_output: await_output.try_into()?,
            }),
            RemoteProcessStatus::Failed { await_output } => Ok(Self::Failed {
                await_output: await_output.try_into()?,
            }),
            RemoteProcessStatus::Cancelled { await_output } => Ok(Self::Cancelled {
                await_output: await_output.try_into()?,
            }),
            RemoteProcessStatus::Abandoned { await_output } => Ok(Self::Abandoned {
                await_output: await_output.try_into()?,
            }),
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
            lash_core::ProcessInput::Engine { kind, payload } => {
                Ok(Self::Engine { kind, payload })
            }
            lash_core::ProcessInput::SessionTurn {
                create_request,
                turn_input,
                output_contract,
            } => Ok(Self::SessionTurn {
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
                create_request,
                turn_input,
                output_contract,
            } => Ok(Self::SessionTurn {
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

impl From<lash_core::ProcessEventType> for RemoteProcessEventType {
    fn from(value: lash_core::ProcessEventType) -> Self {
        let lash_core::ProcessEventType {
            name,
            payload_schema,
            semantics,
        } = value;
        Self {
            name,
            payload_schema: payload_schema.schema,
            semantics: semantics.into(),
        }
    }
}

impl From<RemoteProcessEventType> for lash_core::ProcessEventType {
    fn from(value: RemoteProcessEventType) -> Self {
        let RemoteProcessEventType {
            name,
            payload_schema,
            semantics,
        } = value;
        Self {
            name,
            payload_schema: lash_core::LashSchema::new(payload_schema),
            semantics: semantics.into(),
        }
    }
}

impl From<lash_core::runtime::ProcessEventSemanticsSpec> for RemoteProcessEventSemanticsSpec {
    fn from(value: lash_core::runtime::ProcessEventSemanticsSpec) -> Self {
        let lash_core::runtime::ProcessEventSemanticsSpec { terminal, wake } = value;
        Self {
            terminal: terminal.map(Into::into),
            wake: wake.map(Into::into),
        }
    }
}

impl From<RemoteProcessEventSemanticsSpec> for lash_core::runtime::ProcessEventSemanticsSpec {
    fn from(value: RemoteProcessEventSemanticsSpec) -> Self {
        let RemoteProcessEventSemanticsSpec { terminal, wake } = value;
        Self {
            terminal: terminal.map(Into::into),
            wake: wake.map(Into::into),
        }
    }
}

impl From<lash_core::ProcessTerminalSpec> for RemoteProcessTerminalSpec {
    fn from(value: lash_core::ProcessTerminalSpec) -> Self {
        let lash_core::ProcessTerminalSpec {
            state,
            await_output,
        } = value;
        Self {
            state: state.into(),
            await_output: await_output.map(Into::into),
        }
    }
}

impl From<RemoteProcessTerminalSpec> for lash_core::ProcessTerminalSpec {
    fn from(value: RemoteProcessTerminalSpec) -> Self {
        let RemoteProcessTerminalSpec {
            state,
            await_output,
        } = value;
        Self {
            state: state.into(),
            await_output: await_output.map(Into::into),
        }
    }
}

impl From<lash_core::ProcessWakeSpec> for RemoteProcessWakeSpec {
    fn from(value: lash_core::ProcessWakeSpec) -> Self {
        let lash_core::ProcessWakeSpec {
            when,
            input,
            dedupe_key,
        } = value;
        Self {
            when: when.map(Into::into),
            input: input.into(),
            dedupe_key: dedupe_key.into(),
        }
    }
}

impl From<RemoteProcessWakeSpec> for lash_core::ProcessWakeSpec {
    fn from(value: RemoteProcessWakeSpec) -> Self {
        let RemoteProcessWakeSpec {
            when,
            input,
            dedupe_key,
        } = value;
        Self {
            when: when.map(Into::into),
            input: input.into(),
            dedupe_key: dedupe_key.into(),
        }
    }
}

impl From<lash_core::runtime::ProcessEventSemantics> for RemoteProcessEventSemantics {
    fn from(value: lash_core::runtime::ProcessEventSemantics) -> Self {
        let lash_core::runtime::ProcessEventSemantics { terminal, wake } = value;
        Self {
            terminal: terminal.map(Into::into),
            wake: wake.map(Into::into),
        }
    }
}

impl TryFrom<RemoteProcessEventSemantics> for lash_core::runtime::ProcessEventSemantics {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessEventSemantics) -> Result<Self, Self::Error> {
        let RemoteProcessEventSemantics { terminal, wake } = value;
        Ok(Self {
            terminal: terminal.map(TryInto::try_into).transpose()?,
            wake: wake.map(Into::into),
        })
    }
}

impl From<lash_core::ProcessTerminalState> for RemoteProcessTerminalState {
    fn from(value: lash_core::ProcessTerminalState) -> Self {
        match value {
            lash_core::ProcessTerminalState::Completed => Self::Completed,
            lash_core::ProcessTerminalState::Failed => Self::Failed,
            lash_core::ProcessTerminalState::Cancelled => Self::Cancelled,
            lash_core::ProcessTerminalState::Abandoned => Self::Abandoned,
        }
    }
}

impl From<RemoteProcessTerminalState> for lash_core::ProcessTerminalState {
    fn from(value: RemoteProcessTerminalState) -> Self {
        match value {
            RemoteProcessTerminalState::Completed => Self::Completed,
            RemoteProcessTerminalState::Failed => Self::Failed,
            RemoteProcessTerminalState::Cancelled => Self::Cancelled,
            RemoteProcessTerminalState::Abandoned => Self::Abandoned,
        }
    }
}

impl From<lash_core::RecoveryDisposition> for RemoteRecoveryDisposition {
    fn from(value: lash_core::RecoveryDisposition) -> Self {
        match value {
            lash_core::RecoveryDisposition::Rerunnable => Self::Rerunnable,
            lash_core::RecoveryDisposition::OwnerBound => Self::OwnerBound,
            lash_core::RecoveryDisposition::ExternallyOwned => Self::ExternallyOwned,
        }
    }
}

impl From<RemoteRecoveryDisposition> for lash_core::RecoveryDisposition {
    fn from(value: RemoteRecoveryDisposition) -> Self {
        match value {
            RemoteRecoveryDisposition::Rerunnable => Self::Rerunnable,
            RemoteRecoveryDisposition::OwnerBound => Self::OwnerBound,
            RemoteRecoveryDisposition::ExternallyOwned => Self::ExternallyOwned,
        }
    }
}

impl From<lash_core::AbandonWriter> for RemoteAbandonWriter {
    fn from(value: lash_core::AbandonWriter) -> Self {
        match value {
            lash_core::AbandonWriter::OwnerDrain => Self::OwnerDrain,
            lash_core::AbandonWriter::Sweep => Self::Sweep,
            lash_core::AbandonWriter::ReconciledRequest => Self::ReconciledRequest,
            lash_core::AbandonWriter::EngineGaveUp => Self::EngineGaveUp,
        }
    }
}

impl From<RemoteAbandonWriter> for lash_core::AbandonWriter {
    fn from(value: RemoteAbandonWriter) -> Self {
        match value {
            RemoteAbandonWriter::OwnerDrain => Self::OwnerDrain,
            RemoteAbandonWriter::Sweep => Self::Sweep,
            RemoteAbandonWriter::ReconciledRequest => Self::ReconciledRequest,
            RemoteAbandonWriter::EngineGaveUp => Self::EngineGaveUp,
        }
    }
}

impl From<lash_core::AbandonEvidence> for RemoteAbandonEvidence {
    fn from(value: lash_core::AbandonEvidence) -> Self {
        let lash_core::AbandonEvidence {
            writer,
            owner,
            epoch_ms,
        } = value;
        Self {
            writer: writer.into(),
            owner: owner.map(|owner| {
                serde_json::to_value(owner).expect("lease owner identity serializes")
            }),
            epoch_ms,
        }
    }
}

impl TryFrom<RemoteAbandonEvidence> for lash_core::AbandonEvidence {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteAbandonEvidence) -> Result<Self, Self::Error> {
        let RemoteAbandonEvidence {
            writer,
            owner,
            epoch_ms,
        } = value;
        Ok(Self {
            writer: writer.into(),
            owner: owner
                .map(|owner| decode_remote_json(owner, "RemoteAbandonEvidence", "owner"))
                .transpose()?,
            epoch_ms,
        })
    }
}

impl From<lash_core::ProcessStarted> for RemoteProcessStarted {
    fn from(value: lash_core::ProcessStarted) -> Self {
        let lash_core::ProcessStarted {
            owner,
            fencing_token,
            attempt,
            started_at_ms,
        } = value;
        Self {
            owner: serde_json::to_value(owner).expect("lease owner identity serializes"),
            fencing_token,
            attempt,
            started_at_ms,
        }
    }
}

impl TryFrom<RemoteProcessStarted> for lash_core::ProcessStarted {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessStarted) -> Result<Self, Self::Error> {
        let RemoteProcessStarted {
            owner,
            fencing_token,
            attempt,
            started_at_ms,
        } = value;
        Ok(Self {
            owner: decode_remote_json(owner, "RemoteProcessStarted", "owner")?,
            fencing_token,
            attempt,
            started_at_ms,
        })
    }
}

impl From<lash_core::AbandonRequest> for RemoteAbandonRequest {
    fn from(value: lash_core::AbandonRequest) -> Self {
        let lash_core::AbandonRequest {
            requested_by,
            requested_at_ms,
            reason,
        } = value;
        Self {
            requested_by,
            requested_at_ms,
            reason,
        }
    }
}

impl From<RemoteAbandonRequest> for lash_core::AbandonRequest {
    fn from(value: RemoteAbandonRequest) -> Self {
        let RemoteAbandonRequest {
            requested_by,
            requested_at_ms,
            reason,
        } = value;
        Self {
            requested_by,
            requested_at_ms,
            reason,
        }
    }
}

impl From<lash_core::ProcessTerminalSemantics> for RemoteProcessTerminalSemantics {
    fn from(value: lash_core::ProcessTerminalSemantics) -> Self {
        let lash_core::ProcessTerminalSemantics {
            state,
            await_output,
        } = value;
        Self {
            state: state.into(),
            await_output: await_output.into(),
        }
    }
}

impl TryFrom<RemoteProcessTerminalSemantics> for lash_core::ProcessTerminalSemantics {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessTerminalSemantics) -> Result<Self, Self::Error> {
        let RemoteProcessTerminalSemantics {
            state,
            await_output,
        } = value;
        Ok(Self {
            state: state.into(),
            await_output: await_output.try_into()?,
        })
    }
}

impl From<lash_core::ProcessWake> for RemoteProcessWake {
    fn from(value: lash_core::ProcessWake) -> Self {
        let lash_core::ProcessWake { input, dedupe_key } = value;
        Self { input, dedupe_key }
    }
}

impl From<RemoteProcessWake> for lash_core::ProcessWake {
    fn from(value: RemoteProcessWake) -> Self {
        let RemoteProcessWake { input, dedupe_key } = value;
        Self { input, dedupe_key }
    }
}

impl From<lash_core::ProcessWakeDedupeKey> for RemoteProcessWakeDedupeKey {
    fn from(value: lash_core::ProcessWakeDedupeKey) -> Self {
        match value {
            lash_core::ProcessWakeDedupeKey::EventIdentity => Self::EventIdentity,
            lash_core::ProcessWakeDedupeKey::Selector(selector) => Self::Selector(selector.into()),
            lash_core::ProcessWakeDedupeKey::Const(value) => Self::Const(value),
        }
    }
}

impl From<RemoteProcessWakeDedupeKey> for lash_core::ProcessWakeDedupeKey {
    fn from(value: RemoteProcessWakeDedupeKey) -> Self {
        match value {
            RemoteProcessWakeDedupeKey::EventIdentity => Self::EventIdentity,
            RemoteProcessWakeDedupeKey::Selector(selector) => Self::Selector(selector.into()),
            RemoteProcessWakeDedupeKey::Const(value) => Self::Const(value),
        }
    }
}

impl From<lash_core::ProcessValueSelector> for RemoteProcessValueSelector {
    fn from(value: lash_core::ProcessValueSelector) -> Self {
        match value {
            lash_core::ProcessValueSelector::Payload => Self::Payload,
            lash_core::ProcessValueSelector::Pointer(value) => Self::Pointer(value),
            lash_core::ProcessValueSelector::Const(value) => Self::Const(value),
            lash_core::ProcessValueSelector::Template { template, fields } => Self::Template {
                template,
                fields: fields
                    .into_iter()
                    .map(|(name, selector)| (name, selector.into()))
                    .collect(),
            },
            lash_core::ProcessValueSelector::Present(value) => Self::Present(value),
        }
    }
}

impl From<RemoteProcessValueSelector> for lash_core::ProcessValueSelector {
    fn from(value: RemoteProcessValueSelector) -> Self {
        match value {
            RemoteProcessValueSelector::Payload => Self::Payload,
            RemoteProcessValueSelector::Pointer(value) => Self::Pointer(value),
            RemoteProcessValueSelector::Const(value) => Self::Const(value),
            RemoteProcessValueSelector::Template { template, fields } => Self::Template {
                template,
                fields: fields
                    .into_iter()
                    .map(|(name, selector)| (name, selector.into()))
                    .collect(),
            },
            RemoteProcessValueSelector::Present(value) => Self::Present(value),
        }
    }
}

impl From<lash_core::RuntimeInvocation> for RemoteRuntimeInvocation {
    fn from(value: lash_core::RuntimeInvocation) -> Self {
        let lash_core::RuntimeInvocation {
            scope,
            subject,
            caused_by,
            replay,
        } = value;
        Self {
            scope: scope.into(),
            subject: subject.into(),
            caused_by: caused_by.map(Into::into),
            replay: replay.map(Into::into),
        }
    }
}

impl From<RemoteRuntimeInvocation> for lash_core::RuntimeInvocation {
    fn from(value: RemoteRuntimeInvocation) -> Self {
        let RemoteRuntimeInvocation {
            scope,
            subject,
            caused_by,
            replay,
        } = value;
        Self {
            scope: scope.into(),
            subject: subject.into(),
            caused_by: caused_by.map(Into::into),
            replay: replay.map(Into::into),
        }
    }
}

impl From<lash_core::runtime::RuntimeScope> for RemoteRuntimeScope {
    fn from(value: lash_core::runtime::RuntimeScope) -> Self {
        let lash_core::runtime::RuntimeScope {
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        } = value;
        Self {
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        }
    }
}

impl From<RemoteRuntimeScope> for lash_core::runtime::RuntimeScope {
    fn from(value: RemoteRuntimeScope) -> Self {
        let RemoteRuntimeScope {
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        } = value;
        Self {
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        }
    }
}

impl From<lash_core::runtime::RuntimeReplay> for RemoteRuntimeReplay {
    fn from(value: lash_core::runtime::RuntimeReplay) -> Self {
        let lash_core::runtime::RuntimeReplay { key } = value;
        Self { key }
    }
}

impl From<RemoteRuntimeReplay> for lash_core::runtime::RuntimeReplay {
    fn from(value: RemoteRuntimeReplay) -> Self {
        let RemoteRuntimeReplay { key } = value;
        Self { key }
    }
}

impl From<lash_core::runtime::RuntimeSubject> for RemoteRuntimeSubject {
    fn from(value: lash_core::runtime::RuntimeSubject) -> Self {
        match value {
            lash_core::runtime::RuntimeSubject::Effect { effect_id, kind } => Self::Effect {
                effect_id,
                kind: kind.into(),
            },
            lash_core::runtime::RuntimeSubject::Process { process_id } => Self::Process { process_id },
            lash_core::runtime::RuntimeSubject::ProcessEvent {
                process_id,
                sequence,
                event_type,
            } => Self::ProcessEvent {
                process_id,
                sequence,
                event_type,
            },
            lash_core::runtime::RuntimeSubject::TriggerOccurrence { occurrence_id } => {
                Self::TriggerOccurrence { occurrence_id }
            }
            lash_core::runtime::RuntimeSubject::SessionNode { node_id } => Self::SessionNode { node_id },
        }
    }
}

impl From<RemoteRuntimeSubject> for lash_core::runtime::RuntimeSubject {
    fn from(value: RemoteRuntimeSubject) -> Self {
        match value {
            RemoteRuntimeSubject::Effect { effect_id, kind } => Self::Effect {
                effect_id,
                kind: kind.into(),
            },
            RemoteRuntimeSubject::Process { process_id } => Self::Process { process_id },
            RemoteRuntimeSubject::ProcessEvent {
                process_id,
                sequence,
                event_type,
            } => Self::ProcessEvent {
                process_id,
                sequence,
                event_type,
            },
            RemoteRuntimeSubject::TriggerOccurrence { occurrence_id } => {
                Self::TriggerOccurrence { occurrence_id }
            }
            RemoteRuntimeSubject::SessionNode { node_id } => Self::SessionNode { node_id },
        }
    }
}

impl From<lash_core::RuntimeEffectKind> for RemoteRuntimeEffectKind {
    fn from(value: lash_core::RuntimeEffectKind) -> Self {
        match value {
            lash_core::RuntimeEffectKind::LlmCall => Self::LlmCall,
            lash_core::RuntimeEffectKind::Direct => Self::Direct,
            lash_core::RuntimeEffectKind::ToolAttempt => Self::ToolAttempt,
            lash_core::RuntimeEffectKind::ToolBatch => Self::ToolBatch,
            lash_core::RuntimeEffectKind::Process => Self::Process,
            lash_core::RuntimeEffectKind::Trigger => Self::Trigger,
            lash_core::RuntimeEffectKind::ExecCode => Self::ExecCode,
            lash_core::RuntimeEffectKind::Checkpoint => Self::Checkpoint,
            lash_core::RuntimeEffectKind::SyncExecutionEnvironment => Self::SyncExecutionEnvironment,
            lash_core::RuntimeEffectKind::Sleep => Self::Sleep,
            lash_core::RuntimeEffectKind::AwaitEvent => Self::AwaitEvent,
            lash_core::RuntimeEffectKind::PeekAwaitEvent => Self::PeekAwaitEvent,
        }
    }
}

impl From<RemoteRuntimeEffectKind> for lash_core::RuntimeEffectKind {
    fn from(value: RemoteRuntimeEffectKind) -> Self {
        match value {
            RemoteRuntimeEffectKind::LlmCall => Self::LlmCall,
            RemoteRuntimeEffectKind::Direct => Self::Direct,
            RemoteRuntimeEffectKind::ToolAttempt => Self::ToolAttempt,
            RemoteRuntimeEffectKind::ToolBatch => Self::ToolBatch,
            RemoteRuntimeEffectKind::Process => Self::Process,
            RemoteRuntimeEffectKind::Trigger => Self::Trigger,
            RemoteRuntimeEffectKind::ExecCode => Self::ExecCode,
            RemoteRuntimeEffectKind::Checkpoint => Self::Checkpoint,
            RemoteRuntimeEffectKind::SyncExecutionEnvironment => Self::SyncExecutionEnvironment,
            RemoteRuntimeEffectKind::Sleep => Self::Sleep,
            RemoteRuntimeEffectKind::AwaitEvent => Self::AwaitEvent,
            RemoteRuntimeEffectKind::PeekAwaitEvent => Self::PeekAwaitEvent,
        }
    }
}

impl From<lash_core::PluginOptions> for RemoteProcessPluginOptions {
    fn from(value: lash_core::PluginOptions) -> Self {
        let lash_core::PluginOptions { plugins } = value;
        Self { plugins }
    }
}

impl From<RemoteProcessPluginOptions> for lash_core::PluginOptions {
    fn from(value: RemoteProcessPluginOptions) -> Self {
        let RemoteProcessPluginOptions { plugins } = value;
        Self { plugins }
    }
}

impl From<lash_core::ModelLimits> for RemoteProcessModelLimits {
    fn from(value: lash_core::ModelLimits) -> Self {
        Self {
            context_window_tokens: value.context_window_tokens.get(),
            output_token_capacity: value.output_token_capacity.map(|value| value.get()),
        }
    }
}

impl From<lash_core::ModelSpec> for RemoteProcessModelSpec {
    fn from(value: lash_core::ModelSpec) -> Self {
        let lash_core::ModelSpec {
            id,
            variant,
            capability,
            limits,
        } = value;
        Self {
            id,
            variant: variant.into(),
            capability: capability.into(),
            limits: limits.into(),
        }
    }
}

impl TryFrom<RemoteProcessModelSpec> for lash_core::ModelSpec {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessModelSpec) -> Result<Self, Self::Error> {
        let RemoteProcessModelSpec {
            id,
            variant,
            capability,
            limits,
        } = value;
        Ok(lash_core::ModelSpec::from_token_limits(
            id,
            variant.into(),
            limits.context_window_tokens,
            limits.output_token_capacity,
        )
        .map_err(|err| RemoteProtocolError::InvalidEnvelope {
            type_name: "RemoteProcessExecutionPolicy",
            message: err,
        })?
        .with_capability(capability.into()))
    }
}

impl From<lash_core::SessionPolicy> for RemoteProcessExecutionPolicy {
    fn from(value: lash_core::SessionPolicy) -> Self {
        let lash_core::SessionPolicy {
            model,
            provider_id,
            session_id,
            autonomous,
            max_turns,
            prompt,
            generation,
        } = value;
        Self {
            model: model.into(),
            provider_id,
            session_id,
            autonomous,
            max_turns,
            prompt: prompt.into(),
            generation: generation.into(),
        }
    }
}

impl TryFrom<RemoteProcessExecutionPolicy> for lash_core::SessionPolicy {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessExecutionPolicy) -> Result<Self, Self::Error> {
        let RemoteProcessExecutionPolicy {
            model,
            provider_id,
            session_id,
            autonomous,
            max_turns,
            prompt,
            generation,
        } = value;
        Ok(Self {
            model: model.try_into()?,
            provider_id,
            session_id,
            autonomous,
            max_turns,
            prompt: prompt.into(),
            generation: generation.try_into()?,
        })
    }
}

impl From<lash_core::ProcessExecutionEnvSpec> for RemoteProcessExecutionEnvSpec {
    fn from(value: lash_core::ProcessExecutionEnvSpec) -> Self {
        let lash_core::ProcessExecutionEnvSpec {
            plugin_options,
            policy,
        } = value;
        Self {
            plugin_options: plugin_options.into(),
            policy: policy.into(),
        }
    }
}

impl TryFrom<RemoteProcessExecutionEnvSpec> for lash_core::ProcessExecutionEnvSpec {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessExecutionEnvSpec) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessExecutionEnvSpec")?;
        let RemoteProcessExecutionEnvSpec {
            plugin_options,
            policy,
        } = value;
        Ok(Self {
            plugin_options: plugin_options.into(),
            policy: policy.try_into()?,
        })
    }
}

impl From<lash_core::ProcessEvent> for RemoteProcessEvent {
    fn from(value: lash_core::ProcessEvent) -> Self {
        let lash_core::ProcessEvent {
            process_id,
            sequence,
            event_type,
            payload,
            invocation,
            semantics,
            occurred_at,
        } = value;
        Self {
            process_id,
            sequence,
            event_type,
            payload,
            invocation: Some(invocation.into()),
            semantics: semantics.into(),
            occurred_at_ms: lash_core::epoch_ms_from_system_time(occurred_at),
        }
    }
}

impl TryFrom<RemoteProcessEvent> for lash_core::ProcessEvent {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessEvent) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessEvent")?;
        let RemoteProcessEvent {
            process_id,
            sequence,
            event_type,
            payload,
            invocation,
            semantics,
            occurred_at_ms,
        } = value;
        let invocation = invocation.ok_or_else(|| RemoteProtocolError::InvalidEnvelope {
            type_name: "RemoteProcessEvent",
            message: "invocation is required to convert to core ProcessEvent".to_string(),
        })?;
        Ok(Self {
            process_id,
            sequence,
            event_type,
            payload,
            invocation: invocation.into(),
            semantics: semantics.try_into()?,
            occurred_at: lash_core::system_time_from_epoch_ms(occurred_at_ms),
        })
    }
}

impl From<lash_core::ObservedProcessEvent> for RemoteObservedProcessEvent {
    fn from(value: lash_core::ObservedProcessEvent) -> Self {
        let lash_core::ObservedProcessEvent {
            sequence,
            event_type,
            occurred_at_ms,
            payload,
        } = value;
        Self {
            sequence,
            event_type,
            occurred_at_ms,
            payload,
        }
    }
}

impl From<RemoteObservedProcessEvent> for lash_core::ObservedProcessEvent {
    fn from(value: RemoteObservedProcessEvent) -> Self {
        let RemoteObservedProcessEvent {
            sequence,
            event_type,
            occurred_at_ms,
            payload,
        } = value;
        Self {
            sequence,
            event_type,
            occurred_at_ms,
            payload,
        }
    }
}

impl From<lash_core::ProcessHandleSummary> for RemoteProcessSummary {
    fn from(value: lash_core::ProcessHandleSummary) -> Self {
        let lash_core::ProcessHandleSummary {
            handle_type,
            id,
            process_id,
            descriptor,
            definition,
            status,
        } = value;
        Self {
            handle_type,
            id,
            process_id,
            descriptor: descriptor.into(),
            definition: definition.map(Into::into),
            status: status.into(),
        }
    }
}

impl TryFrom<RemoteProcessSummary> for lash_core::ProcessHandleSummary {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessSummary) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessSummary")?;
        let RemoteProcessSummary {
            handle_type,
            id,
            process_id,
            descriptor,
            definition,
            status,
        } = value;
        Ok(Self {
            handle_type,
            id,
            process_id,
            descriptor: descriptor.into(),
            definition: definition.map(Into::into),
            status: status.into(),
        })
    }
}

impl TryFrom<lash_core::ProcessRecord> for RemoteProcessRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessRecord) -> Result<Self, Self::Error> {
        let lash_core::ProcessRecord {
            id,
            registration_hash: _,
            input,
            disposition,
            max_attempts,
            identity,
            event_types,
            provenance,
            env_ref,
            wake_target,
            created_at_ms,
            updated_at_ms,
            external_ref,
            first_started,
            abandon_request,
            wait,
            status,
        } = value;
        Ok(Self {
            process_id: id,
            input: input.as_ref().clone().try_into()?,
            disposition: disposition.into(),
            max_attempts,
            identity: identity.into(),
            event_types: event_types.into_iter().map(Into::into).collect(),
            provenance: provenance.into(),
            env_ref: env_ref
                .map(|env_ref| env_ref.as_str().parse())
                .transpose()?,
            wake_target: wake_target.map(Into::into),
            created_at_ms,
            updated_at_ms,
            external_ref: external_ref.map(Into::into),
            first_started: first_started.map(|started| (*started).into()),
            abandon_request: abandon_request.map(|request| (*request).into()),
            wait: wait.map(Into::into),
            status: status.into(),
        })
    }
}

impl TryFrom<RemoteProcessRecord> for lash_core::ProcessRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessRecord) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessRecord")?;
        let RemoteProcessRecord {
            process_id,
            input,
            disposition,
            max_attempts,
            identity,
            event_types,
            provenance,
            env_ref,
            wake_target,
            created_at_ms,
            updated_at_ms,
            external_ref,
            first_started,
            abandon_request,
            wait,
            status,
        } = value;
        let registration = lash_core::ProcessRegistration::new(
            process_id,
            input.try_into()?,
            disposition.into(),
            provenance.into(),
        )
        .with_max_attempts(max_attempts)
        .with_identity(identity.into())
        .with_event_types(event_types.into_iter().map(Into::into))
        .with_execution_env_ref(env_ref.map(|env_ref| {
            lash_core::ProcessExecutionEnvRef::new(env_ref.as_str().to_string())
        }))
        .with_wake_target(wake_target.map(Into::into));
        let mut record = lash_core::ProcessRecord::from_registration(registration);
        record.created_at_ms = created_at_ms;
        record.updated_at_ms = updated_at_ms;
        record.external_ref = external_ref.map(Into::into);
        record.first_started = first_started
            .map(|started| started.try_into().map(Box::new))
            .transpose()?;
        record.abandon_request = abandon_request.map(|request| Box::new(request.into()));
        record.wait = wait.map(Into::into);
        record.status = status.try_into()?;
        Ok(record)
    }
}

impl TryFrom<lash_core::ObservedProcess> for RemoteObservedProcess {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ObservedProcess) -> Result<Self, Self::Error> {
        let lash_core::ObservedProcess {
            process_id,
            graph_key,
            kind,
            identity,
            lifecycle,
            status_label,
            terminal,
            disposition,
            error,
            created_at_ms,
            updated_at_ms,
            first_started,
            lease_holder,
            lease_expires_at_ms,
            abandon_request,
            input,
            originator,
            env_ref,
            wake_target,
            caused_by,
            external_ref,
            wait,
            child_session_id,
            label,
        } = value;
        Ok(Self {
            process_id,
            graph_key,
            kind,
            identity: identity.into(),
            lifecycle: lifecycle.into(),
            status_label,
            terminal,
            disposition: disposition.into(),
            error,
            created_at_ms,
            updated_at_ms,
            first_started: first_started.map(Into::into),
            lease_holder: lease_holder.map(|owner| {
                serde_json::to_value(owner).expect("lease owner identity serializes")
            }),
            lease_expires_at_ms,
            abandon_request: abandon_request.map(Into::into),
            input: input.try_into()?,
            originator: originator.into(),
            env_ref: env_ref
                .map(|env_ref| env_ref.as_str().parse())
                .transpose()?,
            wake_target: wake_target.map(Into::into),
            caused_by: caused_by.map(Into::into),
            external_ref: external_ref.map(Into::into),
            wait: wait.map(Into::into),
            child_session_id,
            label,
        })
    }
}

impl TryFrom<RemoteObservedProcess> for lash_core::ObservedProcess {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteObservedProcess) -> Result<Self, Self::Error> {
        value.validate("RemoteObservedProcess")?;
        let RemoteObservedProcess {
            process_id,
            graph_key,
            kind,
            identity,
            lifecycle,
            status_label,
            terminal,
            disposition,
            error,
            created_at_ms,
            updated_at_ms,
            first_started,
            lease_holder,
            lease_expires_at_ms,
            abandon_request,
            input,
            originator,
            env_ref,
            wake_target,
            caused_by,
            external_ref,
            wait,
            child_session_id,
            label,
        } = value;
        Ok(Self {
            process_id,
            graph_key,
            kind,
            identity: identity.into(),
            lifecycle: lifecycle.into(),
            status_label,
            terminal,
            disposition: disposition.into(),
            error,
            created_at_ms,
            updated_at_ms,
            first_started: first_started.map(TryInto::try_into).transpose()?,
            lease_holder: lease_holder
                .map(|owner| decode_remote_json(owner, "RemoteObservedProcess", "lease_holder"))
                .transpose()?,
            lease_expires_at_ms,
            abandon_request: abandon_request.map(Into::into),
            input: input.try_into()?,
            originator: originator.into(),
            env_ref: env_ref.map(|env_ref| {
                lash_core::ProcessExecutionEnvRef::new(env_ref.as_str().to_string())
            }),
            wake_target: wake_target.map(Into::into),
            caused_by: caused_by.map(Into::into),
            external_ref: external_ref.map(Into::into),
            wait: wait.map(Into::into),
            child_session_id,
            label,
        })
    }
}

impl TryFrom<lash_core::ObservedWorkItem> for RemoteProcessWorkItem {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ObservedWorkItem) -> Result<Self, Self::Error> {
        let lash_core::ObservedWorkItem {
            process,
            descriptor,
            events,
            kind,
            label,
        } = value;
        Ok(Self {
            process: process.try_into()?,
            descriptor: descriptor.into(),
            events: events.into_iter().map(Into::into).collect(),
            kind,
            label,
        })
    }
}

impl TryFrom<RemoteProcessWorkItem> for lash_core::ObservedWorkItem {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessWorkItem) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessWorkItem")?;
        let RemoteProcessWorkItem {
            process,
            descriptor,
            events,
            kind,
            label,
        } = value;
        Ok(Self {
            process: process.try_into()?,
            descriptor: descriptor.into(),
            events: events.into_iter().map(Into::into).collect(),
            kind,
            label,
        })
    }
}

impl TryFrom<lash_core::ProcessWorkSnapshot> for RemoteProcessWorkSnapshot {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessWorkSnapshot) -> Result<Self, Self::Error> {
        let lash_core::ProcessWorkSnapshot {
            session_id,
            visible_process_ids,
            items,
        } = value;
        Ok(Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            session_id,
            visible_process_ids,
            items: items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<RemoteProcessWorkSnapshot> for lash_core::ProcessWorkSnapshot {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessWorkSnapshot) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessWorkSnapshot {
            protocol_version: _,
            session_id,
            visible_process_ids,
            items,
        } = value;
        Ok(Self {
            session_id,
            visible_process_ids,
            items: items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<RemoteProcessStartRequest> for lash_core::ProcessStartRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessStartRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessStartRequest {
            protocol_version: _,
            id,
            input,
            disposition,
            max_attempts,
            env_spec,
            originator,
            wake_target,
            grant,
            event_types,
        } = value;
        let mut request = lash_core::ProcessStartRequest::new(
            id,
            input.try_into()?,
            disposition.into(),
            originator.into(),
        )
        .with_max_attempts(max_attempts)
        .with_wake_target(wake_target.map(Into::into))
        .with_grant(grant.map(Into::into))
        .with_event_types(event_types.into_iter().map(Into::into));
        request.env_spec = env_spec.map(TryInto::try_into).transpose()?;
        Ok(request)
    }
}

impl TryFrom<lash_core::ProcessStartRequest> for RemoteProcessStartRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessStartRequest) -> Result<Self, Self::Error> {
        let lash_core::ProcessStartRequest {
            id,
            input,
            disposition,
            max_attempts,
            env_spec,
            originator,
            wake_target,
            grant,
            event_types,
        } = value;
        Ok(Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            id,
            input: input.try_into()?,
            disposition: disposition.into(),
            max_attempts,
            env_spec: env_spec.map(Into::into),
            originator: originator.into(),
            wake_target: wake_target.map(Into::into),
            grant: grant.map(Into::into),
            event_types: event_types.into_iter().map(Into::into).collect(),
        })
    }
}

impl TryFrom<lash_core::ProcessRecord> for RemoteProcessStartResult {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            record: value.try_into()?,
            summary: None,
        })
    }
}

impl TryFrom<RemoteProcessStartResult> for lash_core::ProcessRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessStartResult) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessStartResult {
            protocol_version: _,
            record,
            summary: _,
        } = value;
        record.try_into()
    }
}

impl From<lash_core::ProcessStatusFilter> for RemoteProcessStatusFilter {
    fn from(value: lash_core::ProcessStatusFilter) -> Self {
        match value {
            lash_core::ProcessStatusFilter::Running => Self::Running,
            lash_core::ProcessStatusFilter::Completed => Self::Completed,
            lash_core::ProcessStatusFilter::Failed => Self::Failed,
            lash_core::ProcessStatusFilter::Cancelled => Self::Cancelled,
            lash_core::ProcessStatusFilter::Abandoned => Self::Abandoned,
            lash_core::ProcessStatusFilter::Any => Self::Any,
        }
    }
}

impl From<RemoteProcessStatusFilter> for lash_core::ProcessStatusFilter {
    fn from(value: RemoteProcessStatusFilter) -> Self {
        match value {
            RemoteProcessStatusFilter::Running => Self::Running,
            RemoteProcessStatusFilter::Completed => Self::Completed,
            RemoteProcessStatusFilter::Failed => Self::Failed,
            RemoteProcessStatusFilter::Cancelled => Self::Cancelled,
            RemoteProcessStatusFilter::Abandoned => Self::Abandoned,
            RemoteProcessStatusFilter::Any => Self::Any,
        }
    }
}

impl TryFrom<RemoteProcessListFilter> for lash_core::ProcessListFilter {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessListFilter) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessListFilter {
            protocol_version: _,
            definition,
            status,
            waiting,
            originator_scope_id,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
        } = value;
        Ok(Self {
            definition: definition.map(Into::into),
            status: status.into(),
            waiting,
            originator_scope_id,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
        })
    }
}

impl From<lash_core::ProcessListFilter> for RemoteProcessListFilter {
    fn from(value: lash_core::ProcessListFilter) -> Self {
        let lash_core::ProcessListFilter {
            definition,
            status,
            waiting,
            originator_scope_id,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
        } = value;
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            definition: definition.map(Into::into),
            status: status.into(),
            waiting,
            originator_scope_id,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
        }
    }
}

impl TryFrom<Vec<lash_core::ObservedProcess>> for RemoteProcessListResponse {
    type Error = RemoteProtocolError;

    fn try_from(value: Vec<lash_core::ObservedProcess>) -> Result<Self, Self::Error> {
        Ok(Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            records: value
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<RemoteProcessListResponse> for Vec<lash_core::ObservedProcess> {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessListResponse) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessListResponse {
            protocol_version: _,
            records,
        } = value;
        records.into_iter().map(TryInto::try_into).collect()
    }
}

impl From<RemoteProcessCancelRequest> for lash_core::ProcessCommand {
    fn from(value: RemoteProcessCancelRequest) -> Self {
        let RemoteProcessCancelRequest {
            protocol_version: _,
            process_id,
            reason,
        } = value;
        Self::Cancel { process_id, reason }
    }
}

impl From<lash_core::ProcessCancelSummary> for RemoteProcessCancelResult {
    fn from(value: lash_core::ProcessCancelSummary) -> Self {
        let lash_core::ProcessCancelSummary { process_id, status } = value;
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id,
            status: status.into(),
            record: None,
        }
    }
}

impl TryFrom<RemoteProcessCancelResult> for lash_core::ProcessCancelSummary {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessCancelResult) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessCancelResult {
            protocol_version: _,
            process_id,
            status,
            record: _,
        } = value;
        Ok(Self {
            process_id,
            status: status.into(),
        })
    }
}

impl TryFrom<RemoteProcessSignalRequest> for lash_core::ProcessEventAppendRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessSignalRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessSignalRequest {
            protocol_version: _,
            process_id: _,
            signal_name,
            signal_id: _,
            payload,
            replay_key,
            wake_target_scope,
        } = value;
        let event_type =
            lash_core::process_signal_event_type(&signal_name).map_err(|err| {
                RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessSignalRequest",
                    message: err.to_string(),
                }
            })?;
        Ok(lash_core::ProcessEventAppendRequest {
            event_type,
            payload,
            replay: replay_key.map(|key| lash_core::runtime::RuntimeReplay { key }),
            wake_target_scope: wake_target_scope.map(Into::into),
        })
    }
}

impl TryFrom<RemoteProcessSignalRequest> for lash_core::ProcessCommand {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessSignalRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let process_id = value.process_id.clone();
        let signal_name = value.signal_name.clone();
        let signal_id = value.signal_id.clone();
        let request = value.try_into()?;
        Ok(Self::Signal {
            process_id,
            signal_name,
            signal_id,
            request,
        })
    }
}

impl From<lash_core::ProcessEvent> for RemoteProcessSignalResult {
    fn from(value: lash_core::ProcessEvent) -> Self {
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            event: value.into(),
        }
    }
}

impl TryFrom<RemoteProcessSignalResult> for lash_core::ProcessEvent {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessSignalResult) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessSignalResult {
            protocol_version: _,
            event,
        } = value;
        event.try_into()
    }
}

impl From<RemoteProcessAwaitRequest> for lash_core::ProcessCommand {
    fn from(value: RemoteProcessAwaitRequest) -> Self {
        let RemoteProcessAwaitRequest {
            protocol_version: _,
            process_id,
        } = value;
        Self::Await { process_id }
    }
}

impl From<(String, lash_core::ProcessAwaitOutput)> for RemoteProcessAwaitResult {
    fn from((process_id, output): (String, lash_core::ProcessAwaitOutput)) -> Self {
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id,
            output: output.into(),
        }
    }
}

impl TryFrom<RemoteProcessAwaitResult> for (String, lash_core::ProcessAwaitOutput) {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessAwaitResult) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessAwaitResult {
            protocol_version: _,
            process_id,
            output,
        } = value;
        Ok((process_id, output.try_into()?))
    }
}

impl From<(String, Vec<lash_core::ProcessEvent>)> for RemoteProcessEventsResponse {
    fn from((process_id, events): (String, Vec<lash_core::ProcessEvent>)) -> Self {
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id,
            events: events.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<RemoteProcessEventsResponse> for (String, Vec<lash_core::ProcessEvent>) {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessEventsResponse) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessEventsResponse {
            protocol_version: _,
            process_id,
            events,
        } = value;
        Ok((
            process_id,
            events
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        ))
    }
}
