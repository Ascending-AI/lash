use super::*;

impl From<lash_core::runtime::AdmissionBoundary> for RemoteAdmissionBoundary {
    fn from(value: lash_core::runtime::AdmissionBoundary) -> Self {
        match value {
            lash_core::runtime::AdmissionBoundary::ActiveTurnCheckpoint => {
                Self::ActiveTurnCheckpoint
            }
            lash_core::runtime::AdmissionBoundary::Idle => Self::Idle,
        }
    }
}

impl From<lash_core::TurnOutputSource> for RemoteTurnOutputSource {
    fn from(value: lash_core::TurnOutputSource) -> Self {
        match value {
            lash_core::TurnOutputSource::Runtime => Self::Runtime,
            lash_core::TurnOutputSource::Plugin { plugin_id } => Self::Plugin { plugin_id },
        }
    }
}

impl From<lash_core::MessageOrigin> for RemoteMessageOrigin {
    fn from(value: lash_core::MessageOrigin) -> Self {
        match value {
            lash_core::MessageOrigin::Plugin {
                plugin_id,
                transient,
            } => Self::Plugin {
                plugin_id,
                transient,
            },
            lash_core::MessageOrigin::Process {
                process_id,
                event_type,
                sequence,
                wake_id,
                caused_by,
            } => Self::Process {
                process_id,
                event_type,
                sequence,
                wake_id,
                caused_by: caused_by.map(Into::into),
            },
            lash_core::MessageOrigin::TurnInput { turn_id, input_id } => Self::TurnInput {
                turn_id,
                input_id: input_id.map(lash_core::InputId::into_inner),
            },
            lash_core::MessageOrigin::TurnOutput { turn_id, source } => Self::TurnOutput {
                turn_id,
                source: source.into(),
            },
            _ => Self::Unrecognized,
        }
    }
}

impl From<lash_core::TurnCause> for RemoteTurnCause {
    fn from(value: lash_core::TurnCause) -> Self {
        let lash_core::TurnCause {
            id,
            event_type,
            origin,
            text,
        } = value;
        Self {
            id,
            event_type,
            origin: origin.into(),
            text,
        }
    }
}
