use super::*;

impl TryFrom<RemoteProcessStartRequest> for lash_core::ProcessStartRequest {
    type Error = RemoteProtocolError;

    #[expect(
        clippy::expect_used,
        reason = "validate() above refuses a missing lifecycle policy before this unwrap is reachable"
    )]
    fn try_from(value: RemoteProcessStartRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessStartRequest {
            start_key,
            input,
            disposition,
            lifecycle,
            max_attempts,
            env_spec,
            originator,
            identity,
            wake_session_id,
            observers,
            event_types,
        } = value;
        let mut request = lash_core::ProcessStartRequest::new(
            input.try_into()?,
            disposition.into(),
            originator.try_into()?,
            lifecycle
                .expect("validated required lifecycle")
                .try_into()?,
        );
        // A remote caller's key lands in the host namespace, scoped to the
        // start's originator, where no key lash derives for its own starts
        // can reach (ADR 0107).
        if let Some(start_key) = start_key {
            // A record reports its key's digest (`start_key_digest`), never a
            // caller's key. A caller that echoes that digest back as its key
            // would have it hashed again into a different key and silently
            // start a second process, so the digest spelling is refused.
            if lash_core::StartKey::parse(&start_key).is_ok() {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessStartRequest",
                    message: "start_key is a derived start-key digest (a record's \
                              `start_key_digest`); send the caller's own raw key"
                        .to_string(),
                });
            }
            request = request.with_host_start_key(start_key);
        }
        let mut request = request
            .with_max_attempts(max_attempts)
            .with_wake_session_id(wake_session_id)
            .with_observers(observers)
            .with_event_types(event_types.into_iter().map(Into::into));
        if let Some(identity) = identity {
            request = request.with_declared_identity(identity.into());
        }
        request.env_spec = env_spec.map(TryInto::try_into).transpose()?;
        Ok(request)
    }
}

impl TryFrom<lash_core::ProcessStartRequest> for RemoteProcessStartRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessStartRequest) -> Result<Self, Self::Error> {
        let lash_core::ProcessStartRequest {
            start_key,
            input,
            disposition,
            lifecycle,
            max_attempts,
            env_spec,
            originator,
            identity,
            wake_session_id,
            observers,
            event_types,
        } = value;
        // A core key is already derived: its host bytes cannot be recovered,
        // and a derived key must never cross as a caller's key.
        if start_key.is_some() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessStartRequest",
                message:
                    "a derived start key cannot cross the wire; a remote caller sends its own key"
                        .to_string(),
            });
        }
        Ok(Self {
            start_key: None,
            input: input.try_into()?,
            disposition: disposition.into(),
            lifecycle: Some(lifecycle.into()),
            max_attempts,
            env_spec: env_spec.map(Into::into),
            originator: originator.into(),
            identity: identity.map(Into::into),
            wake_session_id,
            observers,
            event_types: event_types.into_iter().map(Into::into).collect(),
        })
    }
}

impl TryFrom<lash_core::ProcessRecord> for RemoteProcessStartReceipt {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            record: value.try_into()?,
            summary: None,
        })
    }
}

impl TryFrom<RemoteProcessStartReceipt> for lash_core::ProcessRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessStartReceipt) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessStartReceipt { record, summary: _ } = value;
        record.try_into()
    }
}

impl From<lash_core::ProcessStatusFilter> for RemoteProcessStatusFilter {
    fn from(value: lash_core::ProcessStatusFilter) -> Self {
        match value {
            lash_core::ProcessStatusFilter::Any => Self::Any,
            lash_core::ProcessStatusFilter::In(statuses) => {
                Self::In(statuses.into_iter().map(Into::into).collect())
            }
        }
    }
}
impl From<RemoteProcessStatusFilter> for lash_core::ProcessStatusFilter {
    fn from(value: RemoteProcessStatusFilter) -> Self {
        match value {
            RemoteProcessStatusFilter::Any => Self::Any,
            RemoteProcessStatusFilter::In(statuses) => {
                Self::In(statuses.into_iter().map(Into::into).collect())
            }
        }
    }
}

impl TryFrom<RemoteProcessListFilter> for lash_core::ProcessListFilter {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessListFilter) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessListFilter {
            definition,
            status,
            originator,
            parent_scope,
            cancel_pending_before_ms,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
            retired_since_ms,
        } = value;
        Ok(Self {
            definition: definition.map(Into::into),
            status: status.into(),
            originator: originator.map(TryInto::try_into).transpose()?,
            parent_scope: parent_scope.map(Into::into),
            cancel_pending_before_ms,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
            retired_since_ms,
        })
    }
}

impl From<lash_core::ProcessListFilter> for RemoteProcessListFilter {
    fn from(value: lash_core::ProcessListFilter) -> Self {
        let lash_core::ProcessListFilter {
            definition,
            status,
            originator,
            parent_scope,
            cancel_pending_before_ms,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
            retired_since_ms,
        } = value;
        Self {
            definition: definition.map(Into::into),
            status: status.into(),
            originator: originator.map(Into::into),
            parent_scope: parent_scope.map(Into::into),
            cancel_pending_before_ms,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
            retired_since_ms,
        }
    }
}

impl TryFrom<Vec<lash_core::facade_support::ObservedProcess>> for RemoteProcessListResponse {
    type Error = RemoteProtocolError;

    fn try_from(
        value: Vec<lash_core::facade_support::ObservedProcess>,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            records: value
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<RemoteProcessListResponse> for Vec<lash_core::facade_support::ObservedProcess> {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessListResponse) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessListResponse { records } = value;
        records.into_iter().map(TryInto::try_into).collect()
    }
}

impl From<RemoteProcessCancelRequest> for lash_core::ProcessCommand {
    fn from(value: RemoteProcessCancelRequest) -> Self {
        let RemoteProcessCancelRequest {
            process_id,
            requester,
        } = value;
        Self::Cancel {
            process_id,
            requester,
            origin: lash_core::CancelOrigin::OperatorRequested,
            attribution: None,
        }
    }
}

impl From<lash_core::ProcessCancelReceipt> for RemoteProcessCancelReceipt {
    fn from(value: lash_core::ProcessCancelReceipt) -> Self {
        let lash_core::ProcessCancelReceipt {
            process_id,
            status,
            origin,
        } = value;
        Self {
            process_id,
            status: status.into(),
            origin,
            record: None,
        }
    }
}

impl TryFrom<RemoteProcessCancelReceipt> for lash_core::ProcessCancelReceipt {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessCancelReceipt) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessCancelReceipt {
            process_id,
            status,
            origin,
            record: _,
        } = value;
        Ok(Self {
            process_id,
            status: status.into(),
            origin,
        })
    }
}

impl TryFrom<RemoteProcessSignalRequest> for lash_core::ProcessEventAppendRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessSignalRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessSignalRequest {
            process_id: _,
            signal_name,
            signal_id: _,
            payload,
            replay_key,
        } = value;
        let event_type = lash_core::facade_support::process_signal_event_type(&signal_name)
            .map_err(|err| RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessSignalRequest",
                message: err.to_string(),
            })?;
        Ok(lash_core::ProcessEventAppendRequest {
            event_type,
            payload,
            replay: replay_key.map(|key| lash_core::runtime::RuntimeReplay {
                key,
                attribution: None,
            }),
            // A remote signal is news to the session by definition: a peer sent
            // it. Only the runtime's own park announcements withhold the wake.
            wake_suppressed: false,
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

impl TryFrom<lash_core::ProcessEvent> for RemoteProcessSignalReceipt {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            event: value.try_into()?,
        })
    }
}

impl TryFrom<RemoteProcessSignalReceipt> for lash_core::ProcessEvent {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessSignalReceipt) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessSignalReceipt { event } = value;
        event.try_into()
    }
}

impl From<RemoteProcessAwaitRequest> for lash_core::ProcessCommand {
    fn from(value: RemoteProcessAwaitRequest) -> Self {
        let RemoteProcessAwaitRequest { process_id } = value;
        Self::Await { process_id }
    }
}

impl TryFrom<(lash_core::ProcessId, lash_core::ProcessAwaitOutput)> for RemoteProcessAwaitOutcome {
    type Error = RemoteProtocolError;

    fn try_from(
        (process_id, output): (lash_core::ProcessId, lash_core::ProcessAwaitOutput),
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            process_id,
            output: output.try_into()?,
        })
    }
}

impl TryFrom<RemoteProcessAwaitOutcome> for (lash_core::ProcessId, lash_core::ProcessAwaitOutput) {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessAwaitOutcome) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessAwaitOutcome { process_id, output } = value;
        Ok((process_id, output.try_into()?))
    }
}

impl
    TryFrom<(
        lash_core::ProcessId,
        lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
        lash_sansio::ProcessCursor,
    )> for RemoteProcessEventsResponse
{
    type Error = RemoteProtocolError;

    fn try_from(
        (process_id, outcome, cursor): (
            lash_core::ProcessId,
            lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
            lash_sansio::ProcessCursor,
        ),
    ) -> Result<Self, Self::Error> {
        let outcome = match outcome {
            lash_core::ProcessEventReadOutcome::NoLongerRetained(retention) => {
                lash_core::ProcessEventReadOutcome::NoLongerRetained(retention)
            }
            lash_core::ProcessEventReadOutcome::Retained(page) => {
                let events = match page.events {
                    lash_core::ProcessEventPageEvents::Full(events) => {
                        lash_core::ProcessEventPageEvents::Full(
                            events
                                .into_iter()
                                .map(TryInto::try_into)
                                .collect::<Result<_, _>>()?,
                        )
                    }
                    lash_core::ProcessEventPageEvents::Lite(events) => {
                        lash_core::ProcessEventPageEvents::Lite(events)
                    }
                };
                lash_core::ProcessEventReadOutcome::Retained(lash_core::ProcessEventPage {
                    events,
                    more: page.more,
                })
            }
        };
        let response = Self {
            process_id,
            outcome,
            cursor,
        };
        response.validate()?;
        Ok(response)
    }
}

impl TryFrom<RemoteProcessEventsResponse>
    for (
        lash_core::ProcessId,
        lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
        lash_sansio::ProcessCursor,
    )
{
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessEventsResponse) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessEventsResponse {
            process_id,
            outcome,
            cursor,
        } = value;
        let outcome = match outcome {
            lash_core::ProcessEventReadOutcome::NoLongerRetained(retention) => {
                lash_core::ProcessEventReadOutcome::NoLongerRetained(retention)
            }
            lash_core::ProcessEventReadOutcome::Retained(page) => {
                let events = match page.events {
                    lash_core::ProcessEventPageEvents::Full(events) => {
                        lash_core::ProcessEventPageEvents::Full(
                            events
                                .into_iter()
                                .map(TryInto::try_into)
                                .collect::<Result<_, _>>()?,
                        )
                    }
                    lash_core::ProcessEventPageEvents::Lite(events) => {
                        lash_core::ProcessEventPageEvents::Lite(events)
                    }
                };
                lash_core::ProcessEventReadOutcome::Retained(lash_core::ProcessEventPage {
                    events,
                    more: page.more,
                })
            }
        };
        Ok((process_id, outcome, cursor))
    }
}

impl From<lash_core::ProcessOriginatorFilter> for RemoteProcessOriginatorFilter {
    fn from(value: lash_core::ProcessOriginatorFilter) -> Self {
        match value {
            lash_core::ProcessOriginatorFilter::Host { scope } => Self::Host { scope },
            lash_core::ProcessOriginatorFilter::Session(scope) => Self::Session(scope.into()),
        }
    }
}

impl TryFrom<RemoteProcessOriginatorFilter> for lash_core::ProcessOriginatorFilter {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessOriginatorFilter) -> Result<Self, Self::Error> {
        Ok(match value {
            RemoteProcessOriginatorFilter::Host { scope } => Self::Host { scope },
            RemoteProcessOriginatorFilter::Session(scope) => Self::Session(scope.try_into()?),
        })
    }
}
