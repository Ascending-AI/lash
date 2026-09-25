use super::*;

impl TryFrom<lash_core::ProcessEvent> for RemoteProcessEvent {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessEvent) -> Result<Self, Self::Error> {
        let lash_core::ProcessEvent {
            process_id,
            process_incarnation,
            sequence,
            event_type,
            payload,
            invocation,
            semantics,
            occurred_at,
        } = value;
        Ok(Self {
            process_id,
            process_incarnation: process_incarnation.registration_sequence(),
            sequence,
            event_type,
            payload,
            invocation: Some(invocation.into()),
            semantics: semantics.try_into()?,
            occurred_at_ms: occurred_at,
        })
    }
}

impl TryFrom<RemoteProcessEvent> for lash_core::ProcessEvent {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessEvent) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessEvent")?;
        let RemoteProcessEvent {
            process_id,
            process_incarnation,
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
            process_incarnation: lash_core::ProcessIncarnation::from_registration_sequence(
                process_incarnation,
            ),
            sequence,
            event_type,
            payload,
            invocation: invocation.into(),
            semantics: semantics.try_into()?,
            occurred_at: occurred_at_ms,
        })
    }
}

impl From<lash_core::facade_support::ObservedProcessEvent> for RemoteObservedProcessEvent {
    fn from(value: lash_core::facade_support::ObservedProcessEvent) -> Self {
        let lash_core::facade_support::ObservedProcessEvent {
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

impl From<RemoteObservedProcessEvent> for lash_core::facade_support::ObservedProcessEvent {
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

impl From<lash_core::ProcessHandleView> for RemoteProcessHandleView {
    fn from(value: lash_core::ProcessHandleView) -> Self {
        let lash_core::ProcessHandleView {
            id,
            process_id,
            incarnation,
            kind,
            label,
            definition,
            status,
            ..
        } = value;
        Self {
            handle_kind: (),
            id: id.to_string(),
            process_id,
            incarnation: incarnation.registration_sequence(),
            kind: kind.into(),
            label,
            definition: definition.map(Into::into),
            status: status.into(),
        }
    }
}

impl TryFrom<RemoteProcessHandleView> for lash_core::ProcessHandleView {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessHandleView) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessHandleView")?;
        let RemoteProcessHandleView {
            id,
            process_id,
            incarnation,
            kind,
            label,
            definition,
            status,
            ..
        } = value;
        // The view is rebuilt from its parts rather than adopting the peer's
        // `id` text, so a handle that arrives naming a different process or
        // incarnation than the fields beside it cannot survive the crossing.
        let rebuilt = lash_core::ProcessHandleView::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            lash_core::ProcessIdentity {
                kind: kind.into(),
                label,
                definition: definition.map(Into::into),
            },
            status.into(),
        );
        if rebuilt.id.as_str() != id {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessHandleView",
                message: "handle id does not name the process and incarnation beside it"
                    .to_string(),
            });
        }
        Ok(rebuilt)
    }
}

impl TryFrom<lash_core::ProcessRecord> for RemoteProcessRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::ProcessRecord) -> Result<Self, Self::Error> {
        let lash_core::ProcessRecord {
            id,
            incarnation,
            last_event_sequence,
            registration_fingerprint: _,
            input,
            disposition,
            lifecycle,
            max_attempts,
            identity,
            event_types,
            provenance,
            env_ref,
            created_at_ms,
            updated_at_ms,
            external_ref,
            first_started,
            abandon_request,
            cancel_request,
            wait,
            park,
            status,
            outcome,
        } = value;
        Ok(Self {
            process_id: id,
            incarnation: incarnation.registration_sequence(),
            last_event_sequence,
            input: input.as_ref().clone().try_into()?,
            disposition: disposition.into(),
            lifecycle: lifecycle.into(),
            max_attempts,
            identity: identity.into(),
            event_types: event_types.into_iter().map(Into::into).collect(),
            provenance: provenance.into(),
            env_ref: env_ref
                .map(|env_ref| env_ref.as_str().parse())
                .transpose()?,
            created_at_ms,
            updated_at_ms,
            external_ref: external_ref.map(Into::into),
            first_started: first_started
                .map(|started| (*started).try_into())
                .transpose()?,
            abandon_request: abandon_request.map(|request| (*request).into()),
            cancel_request: cancel_request.map(|request| *request),
            wait: wait.map(Into::into),
            park: park.map(|park| (*park).try_into()).transpose()?,
            status: status.into(),
            outcome: outcome.map(TryInto::try_into).transpose()?,
        })
    }
}

impl TryFrom<RemoteProcessRecord> for lash_core::ProcessRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessRecord) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessRecord")?;
        let RemoteProcessRecord {
            process_id,
            incarnation,
            last_event_sequence,
            input,
            disposition,
            lifecycle,
            max_attempts,
            identity,
            event_types,
            provenance,
            env_ref,
            created_at_ms,
            updated_at_ms,
            external_ref,
            first_started,
            abandon_request,
            cancel_request,
            wait,
            park,
            status,
            outcome,
        } = value;
        let registration =
            lash_core::ProcessRegistration::new(
                process_id,
                input.try_into()?,
                disposition.into(),
                provenance.try_into()?,
                lifecycle.try_into()?,
            )
            .with_max_attempts(max_attempts)
            .with_admitted_identity(lash_core::AdmittedProcessIdentity::pinned(identity.into()))
            .with_event_types(event_types.into_iter().map(Into::into))
            .with_execution_env_ref(env_ref.map(|env_ref| {
                lash_core::ProcessExecutionEnvRef::new(env_ref.as_str().to_string())
            }));
        // `ProcessRecord::from_registration` `.expect()`s on any core validation
        // error, so peer input must clear core's validator here or a malformed
        // record aborts the host (FIG-2985). Running the core validator itself,
        // rather than mirroring its rules, means the two cannot drift: a new
        // core rule refuses peer input the day it lands. The DTO-level
        // `validate` above still refuses what it can see, so most shapes fail
        // before this point with a field-named message.
        let registration =
            lash_core::runtime::prepare_process_registration(registration).map_err(|error| {
                RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessRecord",
                    message: format!("process registration is not valid: {error}"),
                }
            })?;
        let mut record = lash_core::ProcessRecord::from_registration(
            registration,
            lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
        );
        record.created_at_ms = created_at_ms;
        record.updated_at_ms = updated_at_ms;
        record.last_event_sequence = last_event_sequence;
        record.external_ref = external_ref.map(Into::into);
        record.first_started = first_started
            .map(|started| started.try_into().map(Box::new))
            .transpose()?;
        record.abandon_request = abandon_request.map(|request| Box::new(request.into()));
        record.cancel_request = cancel_request.map(Box::new);
        record.wait = wait.map(Into::into);
        record.park = park.map(|park| park.try_into().map(Box::new)).transpose()?;
        record.status = status.into();
        record.outcome = outcome.map(TryInto::try_into).transpose()?;
        Ok(record)
    }
}

impl TryFrom<lash_core::facade_support::ObservedProcess> for RemoteObservedProcess {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::facade_support::ObservedProcess) -> Result<Self, Self::Error> {
        let lash_core::facade_support::ObservedProcess {
            process_id,
            incarnation,
            last_event_sequence,
            identity,
            lifecycle,
            policy,
            disposition,
            error,
            error_code,
            created_at_ms,
            updated_at_ms,
            first_started,
            lease_holder,
            lease_expires_at_ms,
            abandon_request,
            cancel_request,
            input,
            originator,
            env_ref,
            caused_by,
            external_ref,
            wait,
            park,
            child_session_id,
        } = value;
        Ok(Self {
            process_id,
            incarnation: incarnation.registration_sequence(),
            last_event_sequence,
            identity: identity.into(),
            lifecycle: lifecycle.into(),
            policy: policy.into(),
            disposition: disposition.into(),
            error,
            error_code: error_code.map(Into::into),
            created_at_ms,
            updated_at_ms,
            first_started: first_started.map(TryInto::try_into).transpose()?,
            lease_holder: lease_holder.map(Into::into),
            lease_expires_at_ms,
            abandon_request: abandon_request.map(Into::into),
            cancel_request,
            input: input.try_into()?,
            originator: originator.into(),
            env_ref: env_ref
                .map(|env_ref| env_ref.as_str().parse())
                .transpose()?,
            caused_by: caused_by.map(Into::into),
            external_ref: external_ref.map(Into::into),
            wait: wait.map(Into::into),
            park: park.map(TryInto::try_into).transpose()?,
            child_session_id,
        })
    }
}

impl TryFrom<RemoteObservedProcess> for lash_core::facade_support::ObservedProcess {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteObservedProcess) -> Result<Self, Self::Error> {
        value.validate("RemoteObservedProcess")?;
        let RemoteObservedProcess {
            process_id,
            incarnation,
            last_event_sequence,
            identity,
            lifecycle,
            policy,
            disposition,
            error,
            error_code,
            created_at_ms,
            updated_at_ms,
            first_started,
            lease_holder,
            lease_expires_at_ms,
            abandon_request,
            cancel_request,
            input,
            originator,
            env_ref,
            caused_by,
            external_ref,
            wait,
            park,
            child_session_id,
        } = value;
        Ok(Self {
            process_id,
            incarnation: lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            last_event_sequence,
            identity: identity.into(),
            lifecycle: lifecycle.into(),
            policy: policy.try_into()?,
            disposition: disposition.into(),
            error,
            error_code: error_code.map(Into::into),
            created_at_ms,
            updated_at_ms,
            first_started: first_started.map(TryInto::try_into).transpose()?,
            lease_holder: lease_holder.map(Into::into),
            lease_expires_at_ms,
            abandon_request: abandon_request.map(Into::into),
            cancel_request,
            input: input.try_into()?,
            originator: originator.try_into()?,
            env_ref: env_ref.map(|env_ref| {
                lash_core::ProcessExecutionEnvRef::new(env_ref.as_str().to_string())
            }),
            caused_by: caused_by.map(Into::into),
            external_ref: external_ref.map(Into::into),
            wait: wait.map(Into::into),
            park: park.map(TryInto::try_into).transpose()?,
            child_session_id,
        })
    }
}

impl TryFrom<lash_core::facade_support::ObservedWorkItem> for RemoteProcessWorkItem {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::facade_support::ObservedWorkItem) -> Result<Self, Self::Error> {
        // The coherence fields are computed from the carried record and event
        // tail — core derives them, so nothing is copied that could disagree.
        let event_tail_sequence = value.event_tail_sequence();
        let state = value.state();
        let lash_core::facade_support::ObservedWorkItem { process, events } = value;
        let item = Self {
            process: process.try_into()?,
            events: events.into_iter().map(Into::into).collect(),
            event_tail_sequence,
            state: state.into(),
        };
        item.validate("RemoteProcessWorkItem")?;
        Ok(item)
    }
}

impl TryFrom<RemoteProcessWorkItem> for lash_core::facade_support::ObservedWorkItem {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessWorkItem) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessWorkItem")?;
        // Validation pinned the declared coherence fields to the derivation;
        // core rebuilds them, so the wire spellings are never trusted.
        let RemoteProcessWorkItem {
            process,
            events,
            event_tail_sequence: _,
            state: _,
        } = value;
        Ok(Self {
            process: process.try_into()?,
            events: events.into_iter().map(Into::into).collect(),
        })
    }
}

impl From<lash_core::facade_support::ObservedWorkItemState> for RemoteObservedWorkItemState {
    fn from(value: lash_core::facade_support::ObservedWorkItemState) -> Self {
        match value {
            lash_core::facade_support::ObservedWorkItemState::Coherent => Self::Coherent,
            lash_core::facade_support::ObservedWorkItemState::EventTailMismatch {
                record_sequence,
                event_tail_sequence,
            } => Self::EventTailMismatch {
                record_sequence,
                event_tail_sequence,
            },
        }
    }
}

impl From<RemoteObservedWorkItemState> for lash_core::facade_support::ObservedWorkItemState {
    fn from(value: RemoteObservedWorkItemState) -> Self {
        match value {
            RemoteObservedWorkItemState::Coherent => Self::Coherent,
            RemoteObservedWorkItemState::EventTailMismatch {
                record_sequence,
                event_tail_sequence,
            } => Self::EventTailMismatch {
                record_sequence,
                event_tail_sequence,
            },
        }
    }
}

impl TryFrom<lash_core::facade_support::ProcessWorkSnapshot> for RemoteProcessWorkSnapshot {
    type Error = RemoteProtocolError;

    fn try_from(
        value: lash_core::facade_support::ProcessWorkSnapshot,
    ) -> Result<Self, Self::Error> {
        let lash_core::facade_support::ProcessWorkSnapshot {
            session_id,
            visible_processes,
            items,
        } = value;
        Ok(Self {
            session_id,
            visible_processes: visible_processes.into_iter().map(Into::into).collect(),
            items: items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<RemoteProcessWorkSnapshot> for lash_core::facade_support::ProcessWorkSnapshot {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessWorkSnapshot) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteProcessWorkSnapshot {
            session_id,
            visible_processes,
            items,
        } = value;
        Ok(Self {
            session_id,
            visible_processes: visible_processes.into_iter().map(Into::into).collect(),
            items: items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl From<lash_core::ProcessObserverBy> for RemoteProcessObserverBy {
    fn from(value: lash_core::ProcessObserverBy) -> Self {
        match value {
            lash_core::ProcessObserverBy::Host { operation_id } => Self::Host { operation_id },
        }
    }
}

impl From<RemoteProcessObserverBy> for lash_core::ProcessObserverBy {
    fn from(value: RemoteProcessObserverBy) -> Self {
        match value {
            RemoteProcessObserverBy::Host { operation_id } => Self::Host { operation_id },
        }
    }
}
