use super::*;

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
            status,
            await_output,
        } = value;
        Self {
            status: status.into(),
            await_output: await_output.map(Into::into),
        }
    }
}

impl From<RemoteProcessTerminalSpec> for lash_core::ProcessTerminalSpec {
    fn from(value: RemoteProcessTerminalSpec) -> Self {
        let RemoteProcessTerminalSpec {
            status,
            await_output,
        } = value;
        Self {
            status: status.into(),
            await_output: await_output.map(Into::into),
        }
    }
}

impl From<lash_core::ProcessWakeSpec> for RemoteProcessWakeSpec {
    fn from(value: lash_core::ProcessWakeSpec) -> Self {
        let lash_core::ProcessWakeSpec { when, input } = value;
        Self {
            when: when.map(Into::into),
            input: input.into(),
        }
    }
}

impl From<RemoteProcessWakeSpec> for lash_core::ProcessWakeSpec {
    fn from(value: RemoteProcessWakeSpec) -> Self {
        let RemoteProcessWakeSpec { when, input } = value;
        Self {
            when: when.map(Into::into),
            input: input.into(),
        }
    }
}

impl TryFrom<lash_core::runtime::ProcessEventSemantics> for RemoteProcessEventSemantics {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::runtime::ProcessEventSemantics) -> Result<Self, Self::Error> {
        let lash_core::runtime::ProcessEventSemantics { terminal, wake } = value;
        Ok(Self {
            terminal: terminal.map(TryInto::try_into).transpose()?,
            wake: wake.map(Into::into),
        })
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

impl From<lash_core::RecoveryContract> for RemoteRecoveryContract {
    fn from(value: lash_core::RecoveryContract) -> Self {
        match value {
            lash_core::RecoveryContract::Rerunnable => Self::Rerunnable,
            lash_core::RecoveryContract::OwnerBound => Self::OwnerBound,
            lash_core::RecoveryContract::ExternallyOwned => Self::ExternallyOwned,
        }
    }
}

impl From<RemoteRecoveryContract> for lash_core::RecoveryContract {
    fn from(value: RemoteRecoveryContract) -> Self {
        match value {
            RemoteRecoveryContract::Rerunnable => Self::Rerunnable,
            RemoteRecoveryContract::OwnerBound => Self::OwnerBound,
            RemoteRecoveryContract::ExternallyOwned => Self::ExternallyOwned,
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

impl From<lash_core::LeaseOwnerIdentity> for RemoteLeaseOwnerIdentity {
    fn from(value: lash_core::LeaseOwnerIdentity) -> Self {
        let lash_core::LeaseOwnerIdentity {
            owner_id,
            incarnation_id,
        } = value;
        Self {
            owner_id,
            incarnation_id,
        }
    }
}

impl From<RemoteLeaseOwnerIdentity> for lash_core::LeaseOwnerIdentity {
    fn from(value: RemoteLeaseOwnerIdentity) -> Self {
        Self::opaque(value.owner_id, value.incarnation_id)
    }
}

impl TryFrom<lash_core::AbandonEvidence> for RemoteAbandonEvidence {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::AbandonEvidence) -> Result<Self, Self::Error> {
        let lash_core::AbandonEvidence {
            writer,
            owner,
            epoch_ms,
        } = value;
        Ok(Self {
            writer: writer.into(),
            owner: owner.map(Into::into),
            epoch_ms,
        })
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
            owner: owner.map(Into::into),
            epoch_ms,
        })
    }
}

impl TryFrom<lash_core::ProcessStarted> for RemoteProcessStarted {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessStarted) -> Result<Self, Self::Error> {
        let lash_core::ProcessStarted {
            owner,
            fencing_token,
            attempt,
            started_at_ms,
        } = value;
        Ok(Self {
            owner: owner.into(),
            fencing_token,
            attempt,
            started_at_ms,
        })
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
            owner: owner.into(),
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
