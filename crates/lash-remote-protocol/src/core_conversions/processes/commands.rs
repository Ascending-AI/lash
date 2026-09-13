use super::*;

impl TryFrom<RemoteProcessStartRequest> for lash_core::ProcessStartRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessStartRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessStartRequest {
            id,
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
            id,
            input.try_into()?,
            disposition.into(),
            originator.try_into()?,
            lifecycle
                .expect("validated required lifecycle")
                .try_into()?,
        )
        .with_max_attempts(max_attempts)
        .with_wake_session_id(wake_session_id)
        .with_observers(observers)
        .with_event_types(event_types.into_iter().map(Into::into));
        if let Some(identity) = identity {
            request = request.with_identity(identity.into());
        }
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
            lifecycle,
            max_attempts,
            env_spec,
            originator,
            identity,
            wake_session_id,
            observers,
            event_types,
        } = value;
        Ok(Self {
            id,
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
            originator_id,
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
            originator_id,
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
            originator_id,
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
            originator_id,
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
            incarnation,
            reason,
        } = value;
        Self::Cancel {
            process_ref: lash_core::ProcessRef::new(
                process_id,
                lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            ),
            reason,
            replay: None,
        }
    }
}

impl From<lash_core::ProcessCancelReceipt> for RemoteProcessCancelReceipt {
    fn from(value: lash_core::ProcessCancelReceipt) -> Self {
        let lash_core::ProcessCancelReceipt {
            process_id,
            incarnation,
            status,
        } = value;
        Self {
            process_id,
            incarnation: incarnation.registration_sequence(),
            status: status.into(),
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
            incarnation,
            status,
            record: _,
        } = value;
        Ok(Self {
            process_id,
            incarnation: lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            status: status.into(),
        })
    }
}

impl TryFrom<RemoteProcessSignalRequest> for lash_core::ProcessEventAppendRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessSignalRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessSignalRequest {
            process_id: _,
            incarnation: _,
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
        })
    }
}

impl TryFrom<RemoteProcessSignalRequest> for lash_core::ProcessCommand {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessSignalRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let process_ref = lash_core::ProcessRef::new(
            value.process_id.clone(),
            lash_core::ProcessIncarnation::from_registration_sequence(value.incarnation),
        );
        let signal_name = value.signal_name.clone();
        let signal_id = value.signal_id.clone();
        let request = value.try_into()?;
        Ok(Self::Signal {
            process_ref,
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
        let RemoteProcessAwaitRequest {
            process_id,
            incarnation,
        } = value;
        Self::Await {
            process_ref: lash_core::ProcessRef::new(
                process_id,
                lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            ),
        }
    }
}

impl TryFrom<(lash_core::ProcessRef, lash_core::ProcessAwaitOutput)> for RemoteProcessAwaitOutcome {
    type Error = RemoteProtocolError;

    fn try_from(
        (process_ref, output): (lash_core::ProcessRef, lash_core::ProcessAwaitOutput),
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            process_id: process_ref.process_id,
            incarnation: process_ref.incarnation.registration_sequence(),
            output: output.try_into()?,
        })
    }
}

impl TryFrom<RemoteProcessAwaitOutcome> for (lash_core::ProcessRef, lash_core::ProcessAwaitOutput) {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessAwaitOutcome) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessAwaitOutcome {
            process_id,
            incarnation,
            output,
        } = value;
        Ok((
            lash_core::ProcessRef::new(
                process_id,
                lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            ),
            output.try_into()?,
        ))
    }
}

impl TryFrom<(lash_core::ProcessRef, Vec<lash_core::ProcessEvent>)>
    for RemoteProcessEventsResponse
{
    type Error = RemoteProtocolError;

    fn try_from(
        (process_ref, events): (lash_core::ProcessRef, Vec<lash_core::ProcessEvent>),
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            process_id: process_ref.process_id,
            incarnation: process_ref.incarnation.registration_sequence(),
            events: events
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<RemoteProcessEventsResponse>
    for (lash_core::ProcessRef, Vec<lash_core::ProcessEvent>)
{
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessEventsResponse) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessEventsResponse {
            process_id,
            incarnation,
            events,
        } = value;
        Ok((
            lash_core::ProcessRef::new(
                process_id,
                lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            ),
            events
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        ))
    }
}
