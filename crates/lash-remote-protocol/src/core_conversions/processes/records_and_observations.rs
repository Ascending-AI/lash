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
            handle_type,
            id,
            process_id,
            incarnation,
            kind,
            label,
            definition,
            status,
        } = value;
        Self {
            handle_type,
            id,
            process_id,
            incarnation: incarnation.registration_sequence(),
            kind,
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
            handle_type,
            id,
            process_id,
            incarnation,
            kind,
            label,
            definition,
            status,
        } = value;
        Ok(Self {
            handle_type,
            id,
            process_id,
            incarnation: lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            kind,
            label,
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
            incarnation,
            last_event_sequence,
            registration_fingerprint: _,
            input,
            disposition,
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
            wait,
            status,
            outcome,
        } = value;
        Ok(Self {
            process_id: id,
            incarnation: incarnation.registration_sequence(),
            last_event_sequence,
            input: input.as_ref().clone().try_into()?,
            disposition: disposition.into(),
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
            wait: wait.map(Into::into),
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
            wait,
            status,
            outcome,
        } = value;
        let registration =
            lash_core::ProcessRegistration::new(
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
            }));
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
        record.wait = wait.map(Into::into);
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
            caused_by,
            external_ref,
            wait,
            child_session_id,
            label,
        } = value;
        Ok(Self {
            process_id,
            incarnation: incarnation.registration_sequence(),
            last_event_sequence,
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
            lease_holder: lease_holder.map(Into::into),
            lease_expires_at_ms,
            abandon_request: abandon_request.map(Into::into),
            input: input.try_into()?,
            originator: originator.into(),
            env_ref: env_ref
                .map(|env_ref| env_ref.as_str().parse())
                .transpose()?,
            caused_by: caused_by.map(Into::into),
            external_ref: external_ref.map(Into::into),
            wait: wait.map(Into::into),
            child_session_id,
            label,
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
            graph_key: _,
            kind: _,
            identity,
            lifecycle,
            status_label: _,
            terminal: _,
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
            caused_by,
            external_ref,
            wait,
            child_session_id,
            label: _,
        } = value;
        let graph_key = format!("process:{process_id}:incarnation:{incarnation}");
        let identity: lash_core::ProcessIdentity = identity.into();
        let kind = identity.kind.clone();
        let label = identity.label.clone().unwrap_or_else(|| kind.clone());
        let lifecycle: lash_core::ProcessStatus = lifecycle.into();
        let status_label = lifecycle.label().to_string();
        let terminal = lifecycle.is_terminal();
        Ok(Self {
            process_id,
            incarnation: lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            last_event_sequence,
            graph_key,
            kind,
            identity,
            lifecycle,
            status_label,
            terminal,
            disposition: disposition.into(),
            error,
            created_at_ms,
            updated_at_ms,
            first_started: first_started.map(TryInto::try_into).transpose()?,
            lease_holder: lease_holder.map(Into::into),
            lease_expires_at_ms,
            abandon_request: abandon_request.map(Into::into),
            input: input.try_into()?,
            originator: originator.into(),
            env_ref: env_ref.map(|env_ref| {
                lash_core::ProcessExecutionEnvRef::new(env_ref.as_str().to_string())
            }),
            caused_by: caused_by.map(Into::into),
            external_ref: external_ref.map(Into::into),
            wait: wait.map(Into::into),
            child_session_id,
            label,
        })
    }
}

impl TryFrom<lash_core::facade_support::ObservedWorkItem> for RemoteProcessWorkItem {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::facade_support::ObservedWorkItem) -> Result<Self, Self::Error> {
        let lash_core::facade_support::ObservedWorkItem {
            process,
            events,
            event_tail_sequence,
            kind,
            label,
        } = value;
        Ok(Self {
            process: process.try_into()?,
            events: events.into_iter().map(Into::into).collect(),
            event_tail_sequence,
            kind,
            label,
        })
    }
}

impl TryFrom<RemoteProcessWorkItem> for lash_core::facade_support::ObservedWorkItem {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessWorkItem) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessWorkItem")?;
        let RemoteProcessWorkItem {
            process,
            events,
            event_tail_sequence,
            kind: _,
            label: _,
        } = value;
        let process: lash_core::facade_support::ObservedProcess = process.try_into()?;
        let kind = process.identity.kind.clone();
        let label = process
            .identity
            .label
            .clone()
            .unwrap_or_else(|| kind.clone());
        Ok(Self {
            process,
            events: events.into_iter().map(Into::into).collect(),
            event_tail_sequence,
            kind,
            label,
        })
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
            lash_core::ProcessObserverBy::ForkInheritance => Self::ForkInheritance,
        }
    }
}

impl From<RemoteProcessObserverBy> for lash_core::ProcessObserverBy {
    fn from(value: RemoteProcessObserverBy) -> Self {
        match value {
            RemoteProcessObserverBy::Host { operation_id } => Self::Host { operation_id },
            RemoteProcessObserverBy::ForkInheritance => Self::ForkInheritance,
        }
    }
}

impl From<lash_core::ObserverInheritance> for RemoteObserverInheritance {
    fn from(value: lash_core::ObserverInheritance) -> Self {
        match value {
            lash_core::ObserverInheritance::All => Self::All,
            lash_core::ObserverInheritance::None => Self::None,
            lash_core::ObserverInheritance::Only(process_ids) => Self::Only(process_ids),
        }
    }
}

impl From<RemoteObserverInheritance> for lash_core::ObserverInheritance {
    fn from(value: RemoteObserverInheritance) -> Self {
        match value {
            RemoteObserverInheritance::All => Self::All,
            RemoteObserverInheritance::None => Self::None,
            RemoteObserverInheritance::Only(process_ids) => Self::Only(process_ids),
        }
    }
}
