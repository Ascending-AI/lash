impl From<lash_core::TurnCancellationEvidence> for RemoteTurnCancellationEvidence {
    fn from(value: lash_core::TurnCancellationEvidence) -> Self {
        let lash_core::TurnCancellationEvidence {
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

impl From<RemoteTurnCancellationEvidence> for lash_core::TurnCancellationEvidence {
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
    /// Resolve a routing-only remote request against trusted session state.
    ///
    /// The remote envelope deliberately does not carry `IncarnationId`.
    /// Durable hosts must claim the current store-bound lifetime before
    /// constructing the core cancellation address.
    pub fn try_into_core_for_lifetime(
        self,
        lifetime: &lash_core::SessionLifetime,
    ) -> Result<lash_core::TurnCancelRequest, RemoteProtocolError> {
        self.validate()?;
        let Self {
            protocol_version: _,
            session_id,
            turn_id,
            request_id,
            origin,
            reason,
        } = self;
        Ok(lash_core::TurnCancelRequest {
            address: lash_core::TurnAddress::new_for_lifetime(session_id, lifetime, turn_id),
            request_id,
            origin,
            reason,
        })
    }
}

impl From<lash_core::TurnCancelRequest> for RemoteTurnCancelRequest {
    fn from(value: lash_core::TurnCancelRequest) -> Self {
        let lash_core::TurnCancelRequest {
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

impl From<lash_core::TurnCancelOutcome> for RemoteTurnCancelOutcome {
    fn from(value: lash_core::TurnCancelOutcome) -> Self {
        match value {
            lash_core::TurnCancelOutcome::Requested(cancellation) => Self::Requested {
                cancellation: cancellation.into(),
            },
            lash_core::TurnCancelOutcome::AlreadyRequested(cancellation) => {
                Self::AlreadyRequested {
                    cancellation: cancellation.into(),
                }
            }
            lash_core::TurnCancelOutcome::CompletionWonRace => Self::CompletionWonRace,
            lash_core::TurnCancelOutcome::UnknownOrRevoked => Self::UnknownOrRevoked,
        }
    }
}

impl From<RemoteTurnCancelOutcome> for lash_core::TurnCancelOutcome {
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
