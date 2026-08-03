impl From<lash_core::facade_support::TurnCancellationEvidence> for RemoteTurnCancellationEvidence {
    fn from(value: lash_core::facade_support::TurnCancellationEvidence) -> Self {
        let lash_core::facade_support::TurnCancellationEvidence {
            request_id,
            origin,
            reason,
        } = value;
        Self {
            request_id,
            origin,
            reason,
        }
    }
}

impl From<RemoteTurnCancellationEvidence> for lash_core::facade_support::TurnCancellationEvidence {
    fn from(value: RemoteTurnCancellationEvidence) -> Self {
        let RemoteTurnCancellationEvidence {
            request_id,
            origin,
            reason,
        } = value;
        Self {
            request_id,
            origin,
            reason,
        }
    }
}

impl RemoteTurnCancelRequest {
    /// Resolve a routing-only remote request into the core request.
    pub fn try_into_core(
        self,
    ) -> Result<lash_core::facade_support::TurnCancelRequest, RemoteProtocolError> {
        self.validate()?;
        let Self {
            protocol_version: _,
            session_id,
            turn_id,
            request_id,
            origin,
            reason,
        } = self;
        Ok(lash_core::facade_support::TurnCancelRequest {
            address: lash_core::facade_support::TurnAddress::new(session_id, turn_id),
            request_id,
            origin,
            reason,
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
        } = value;
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            session_id: address.session_id,
            turn_id: address.turn_id,
            request_id,
            origin,
            reason,
        }
    }
}

impl From<lash_core::facade_support::TurnCancelOutcome> for RemoteTurnCancelOutcome {
    fn from(value: lash_core::facade_support::TurnCancelOutcome) -> Self {
        match value {
            lash_core::facade_support::TurnCancelOutcome::Requested(cancellation) => Self::Requested {
                cancellation: cancellation.into(),
            },
            lash_core::facade_support::TurnCancelOutcome::AlreadyRequested(cancellation) => {
                Self::AlreadyRequested {
                    cancellation: cancellation.into(),
                }
            }
            lash_core::facade_support::TurnCancelOutcome::CompletionWonRace => Self::CompletionWonRace,
            lash_core::facade_support::TurnCancelOutcome::UnknownOrRevoked => Self::UnknownOrRevoked,
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
            RemoteTurnCancelOutcome::CompletionWonRace => Self::CompletionWonRace,
            RemoteTurnCancelOutcome::UnknownOrRevoked => Self::UnknownOrRevoked,
        }
    }
}
