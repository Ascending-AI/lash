use super::*;

impl From<lash_core::facade_support::TurnCancelUndeliveredInputPolicy>
    for RemoteTurnCancelUndeliveredInputPolicy
{
    fn from(value: lash_core::facade_support::TurnCancelUndeliveredInputPolicy) -> Self {
        match value {
            lash_core::facade_support::TurnCancelUndeliveredInputPolicy::Defer => Self::Defer,
            lash_core::facade_support::TurnCancelUndeliveredInputPolicy::Drop => Self::Drop,
        }
    }
}

impl From<RemoteTurnCancelUndeliveredInputPolicy>
    for lash_core::facade_support::TurnCancelUndeliveredInputPolicy
{
    fn from(value: RemoteTurnCancelUndeliveredInputPolicy) -> Self {
        match value {
            RemoteTurnCancelUndeliveredInputPolicy::Defer => Self::Defer,
            RemoteTurnCancelUndeliveredInputPolicy::Drop => Self::Drop,
        }
    }
}

impl From<lash_core::facade_support::TurnCancelMode> for RemoteTurnCancelMode {
    fn from(value: lash_core::facade_support::TurnCancelMode) -> Self {
        match value {
            lash_core::facade_support::TurnCancelMode::Immediate => Self::Immediate,
            lash_core::facade_support::TurnCancelMode::AfterStep => Self::AfterStep,
        }
    }
}

impl From<RemoteTurnCancelMode> for lash_core::facade_support::TurnCancelMode {
    fn from(value: RemoteTurnCancelMode) -> Self {
        match value {
            RemoteTurnCancelMode::Immediate => Self::Immediate,
            RemoteTurnCancelMode::AfterStep => Self::AfterStep,
        }
    }
}

impl From<lash_core::facade_support::TurnCancellationEvidence> for RemoteTurnCancellationEvidence {
    fn from(value: lash_core::facade_support::TurnCancellationEvidence) -> Self {
        let lash_core::facade_support::TurnCancellationEvidence {
            request_id,
            origin,
            reason,
            undelivered,
            mode,
            honoured_after_step,
        } = value;
        Self {
            request_id,
            origin,
            reason,
            undelivered: undelivered.into(),
            mode: mode.into(),
            honoured_after_step,
        }
    }
}

impl From<RemoteTurnCancellationEvidence> for lash_core::facade_support::TurnCancellationEvidence {
    fn from(value: RemoteTurnCancellationEvidence) -> Self {
        let RemoteTurnCancellationEvidence {
            request_id,
            origin,
            reason,
            undelivered,
            mode,
            honoured_after_step,
        } = value;
        Self {
            request_id,
            origin,
            reason,
            undelivered: undelivered.into(),
            mode: mode.into(),
            honoured_after_step,
        }
    }
}

impl RemoteTurnCancelRequest {
    pub fn try_into_core(
        self,
    ) -> Result<lash_core::facade_support::TurnCancelRequest, RemoteProtocolError> {
        self.validate()?;
        let Self {
            session_id,
            turn_id,
            request_id,
            origin,
            reason,
            undelivered,
            mode,
        } = self;
        Ok(lash_core::facade_support::TurnCancelRequest {
            address: lash_core::facade_support::TurnAddress::new(session_id, turn_id),
            request_id,
            origin,
            reason,
            undelivered: undelivered.into(),
            mode: mode.into(),
        })
    }
}

impl From<lash_core::facade_support::TurnCancelRequest> for RemoteTurnCancelRequest {
    fn from(value: lash_core::facade_support::TurnCancelRequest) -> Self {
        let lash_core::facade_support::TurnCancelRequest {
            address,
            request_id,
            origin,
            reason,
            undelivered,
            mode,
        } = value;
        Self {
            session_id: address.session_id,
            turn_id: address.turn_id,
            request_id,
            origin,
            reason,
            undelivered: undelivered.into(),
            mode: mode.into(),
        }
    }
}

impl From<lash_core::facade_support::TurnCancelOutcome> for RemoteTurnCancelOutcome {
    fn from(value: lash_core::facade_support::TurnCancelOutcome) -> Self {
        match value {
            lash_core::facade_support::TurnCancelOutcome::Requested(cancellation) => {
                Self::Requested {
                    cancellation: cancellation.into(),
                }
            }
            lash_core::facade_support::TurnCancelOutcome::AlreadyRequested(cancellation) => {
                Self::AlreadyRequested {
                    cancellation: cancellation.into(),
                }
            }
            lash_core::facade_support::TurnCancelOutcome::Escalated(cancellation) => {
                Self::Escalated {
                    cancellation: cancellation.into(),
                }
            }
            lash_core::facade_support::TurnCancelOutcome::PolicyConflict {
                requested,
                accepted,
            } => Self::PolicyConflict {
                requested: requested.into(),
                accepted: accepted.into(),
            },
            lash_core::facade_support::TurnCancelOutcome::Withdrawn { input } => {
                Self::Withdrawn { input_id: input }
            }
            lash_core::facade_support::TurnCancelOutcome::CompletionWonRace => {
                Self::CompletionWonRace
            }
            lash_core::facade_support::TurnCancelOutcome::UnknownOrRevoked => {
                Self::UnknownOrRevoked
            }
        }
    }
}

impl From<RemoteTurnCancelOutcome> for lash_core::facade_support::TurnCancelOutcome {
    fn from(value: RemoteTurnCancelOutcome) -> Self {
        match value {
            RemoteTurnCancelOutcome::Requested { cancellation } => {
                Self::Requested(cancellation.into())
            }
            RemoteTurnCancelOutcome::AlreadyRequested { cancellation } => {
                Self::AlreadyRequested(cancellation.into())
            }
            RemoteTurnCancelOutcome::Escalated { cancellation } => {
                Self::Escalated(cancellation.into())
            }
            RemoteTurnCancelOutcome::PolicyConflict {
                requested,
                accepted,
            } => Self::PolicyConflict {
                requested: requested.into(),
                accepted: accepted.into(),
            },
            RemoteTurnCancelOutcome::Withdrawn { input_id } => Self::Withdrawn { input: input_id },
            RemoteTurnCancelOutcome::CompletionWonRace => Self::CompletionWonRace,
            RemoteTurnCancelOutcome::UnknownOrRevoked => Self::UnknownOrRevoked,
        }
    }
}
