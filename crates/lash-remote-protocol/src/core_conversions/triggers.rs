use super::*;

impl From<lash_core::TriggerSubscriptionLifecycle> for RemoteTriggerSubscriptionLifecycle {
    fn from(value: lash_core::TriggerSubscriptionLifecycle) -> Self {
        match value {
            lash_core::TriggerSubscriptionLifecycle::Enabled => Self::Enabled,
            lash_core::TriggerSubscriptionLifecycle::Disabled => Self::Disabled,
            lash_core::TriggerSubscriptionLifecycle::Tombstoned(deleted_at_ms) => {
                Self::Tombstoned(deleted_at_ms)
            }
        }
    }
}

impl From<RemoteTriggerSubscriptionLifecycle> for lash_core::TriggerSubscriptionLifecycle {
    fn from(value: RemoteTriggerSubscriptionLifecycle) -> Self {
        match value {
            RemoteTriggerSubscriptionLifecycle::Enabled => Self::Enabled,
            RemoteTriggerSubscriptionLifecycle::Disabled => Self::Disabled,
            RemoteTriggerSubscriptionLifecycle::Tombstoned(deleted_at_ms) => {
                Self::Tombstoned(deleted_at_ms)
            }
        }
    }
}

impl From<RemoteTriggerOccurrenceOutcome> for lash_core::TriggerOccurrenceOutcome {
    fn from(value: RemoteTriggerOccurrenceOutcome) -> Self {
        match value {
            RemoteTriggerOccurrenceOutcome::Fired => Self::Fired,
            RemoteTriggerOccurrenceOutcome::Dropped { reason } => Self::Dropped { reason },
        }
    }
}

impl From<lash_core::TriggerOccurrenceOutcome> for RemoteTriggerOccurrenceOutcome {
    fn from(value: lash_core::TriggerOccurrenceOutcome) -> Self {
        match value {
            lash_core::TriggerOccurrenceOutcome::Fired => Self::Fired,
            lash_core::TriggerOccurrenceOutcome::Dropped { reason } => Self::Dropped { reason },
        }
    }
}

impl From<RemoteProtocolTurnOptions> for lash_core::ProtocolTurnOptions {
    fn from(value: RemoteProtocolTurnOptions) -> Self {
        let RemoteProtocolTurnOptions { payload } = value;
        Self::from_payload(payload)
    }
}

impl From<lash_core::ProtocolTurnOptions> for RemoteProtocolTurnOptions {
    fn from(value: lash_core::ProtocolTurnOptions) -> Self {
        let lash_core::ProtocolTurnOptions { payload } = value;
        Self { payload }
    }
}

impl TryFrom<RemoteTriggerOccurrenceRequest> for lash_core::TriggerOccurrenceRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerOccurrenceRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteTriggerOccurrenceRequest {
            source_type,
            source_key,
            payload,
            idempotency_key,
            source,
            session_id,
            outcome,
        } = value;
        let mut request = lash_core::TriggerOccurrenceRequest::new(
            source_type,
            source_key,
            payload,
            idempotency_key,
        );
        request.source = source;
        request.session_id = session_id;
        request.outcome = outcome.into();
        Ok(request)
    }
}

impl From<lash_core::TriggerOccurrenceRequest> for RemoteTriggerOccurrenceRequest {
    fn from(value: lash_core::TriggerOccurrenceRequest) -> Self {
        let lash_core::TriggerOccurrenceRequest {
            source_type,
            source_key,
            payload,
            idempotency_key,
            source,
            session_id,
            outcome,
        } = value;
        Self {
            source_type,
            source_key,
            payload,
            idempotency_key,
            source,
            session_id,
            outcome: outcome.into(),
        }
    }
}

impl From<lash_core::TriggerOccurrenceRecord> for RemoteTriggerOccurrenceRecord {
    fn from(value: lash_core::TriggerOccurrenceRecord) -> Self {
        let lash_core::TriggerOccurrenceRecord {
            occurrence_id,
            source_type,
            source_key,
            payload,
            idempotency_key,
            source,
            session_id,
            outcome,
            occurred_at_ms,
        } = value;
        Self {
            occurrence_id,
            source_type,
            source_key,
            payload,
            idempotency_key,
            source,
            session_id,
            outcome: outcome.into(),
            occurred_at_ms,
        }
    }
}

impl From<RemoteTriggerOccurrenceRecord> for lash_core::TriggerOccurrenceRecord {
    fn from(value: RemoteTriggerOccurrenceRecord) -> Self {
        let RemoteTriggerOccurrenceRecord {
            occurrence_id,
            source_type,
            source_key,
            payload,
            idempotency_key,
            source,
            session_id,
            outcome,
            occurred_at_ms,
        } = value;
        Self {
            occurrence_id,
            source_type,
            source_key,
            payload,
            idempotency_key,
            source,
            session_id,
            outcome: outcome.into(),
            occurred_at_ms,
        }
    }
}

impl From<lash_core::facade_support::TriggerEmitReport> for RemoteTriggerEmitReport {
    fn from(value: lash_core::facade_support::TriggerEmitReport) -> Self {
        let lash_core::facade_support::TriggerEmitReport {
            occurrence_id,
            deliveries,
        } = value;
        Self {
            occurrence_id,
            deliveries: deliveries.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<RemoteTriggerEmitReport> for lash_core::facade_support::TriggerEmitReport {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerEmitReport) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteTriggerEmitReport {
            occurrence_id,
            deliveries,
        } = value;
        Ok(Self {
            occurrence_id,
            deliveries: deliveries.into_iter().map(Into::into).collect(),
        })
    }
}

impl From<lash_core::facade_support::TriggerDeliveryEmitOutcome>
    for RemoteTriggerDeliveryEmitOutcome
{
    fn from(value: lash_core::facade_support::TriggerDeliveryEmitOutcome) -> Self {
        match value {
            lash_core::facade_support::TriggerDeliveryEmitOutcome::Started => Self::Started,
            lash_core::facade_support::TriggerDeliveryEmitOutcome::AlreadyReserved => {
                Self::AlreadyReserved
            }
            lash_core::facade_support::TriggerDeliveryEmitOutcome::Failed { reason } => {
                Self::Failed { reason }
            }
        }
    }
}

impl From<RemoteTriggerDeliveryEmitOutcome>
    for lash_core::facade_support::TriggerDeliveryEmitOutcome
{
    fn from(value: RemoteTriggerDeliveryEmitOutcome) -> Self {
        match value {
            RemoteTriggerDeliveryEmitOutcome::Started => Self::Started,
            RemoteTriggerDeliveryEmitOutcome::AlreadyReserved => Self::AlreadyReserved,
            RemoteTriggerDeliveryEmitOutcome::Failed { reason } => Self::Failed { reason },
        }
    }
}

impl From<lash_core::facade_support::TriggerDeliveryEmitReceipt>
    for RemoteTriggerDeliveryEmitReceipt
{
    fn from(value: lash_core::facade_support::TriggerDeliveryEmitReceipt) -> Self {
        let lash_core::facade_support::TriggerDeliveryEmitReceipt {
            occurrence_id,
            subscription_id,
            process_id,
            outcome,
        } = value;
        Self {
            occurrence_id,
            subscription_id,
            process_id,
            outcome: outcome.into(),
        }
    }
}

impl From<RemoteTriggerDeliveryEmitReceipt>
    for lash_core::facade_support::TriggerDeliveryEmitReceipt
{
    fn from(value: RemoteTriggerDeliveryEmitReceipt) -> Self {
        let RemoteTriggerDeliveryEmitReceipt {
            occurrence_id,
            subscription_id,
            process_id,
            outcome,
        } = value;
        Self {
            occurrence_id,
            subscription_id,
            process_id,
            outcome: outcome.into(),
        }
    }
}

impl TryFrom<RemoteTriggerSubscriptionFilter> for lash_core::TriggerSubscriptionFilter {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerSubscriptionFilter) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteTriggerSubscriptionFilter {
            registrant_scope_id,
            subscription_key,
            name,
            source_type,
            source_key,
            target,
            enabled,
        } = value;
        Ok(Self {
            registrant_scope_id,
            subscription_key,
            name,
            source_type,
            source_key,
            target,
            enabled,
        })
    }
}

impl From<lash_core::TriggerSubscriptionFilter> for RemoteTriggerSubscriptionFilter {
    fn from(value: lash_core::TriggerSubscriptionFilter) -> Self {
        let lash_core::TriggerSubscriptionFilter {
            registrant_scope_id,
            subscription_key,
            name,
            source_type,
            source_key,
            target,
            enabled,
        } = value;
        Self {
            registrant_scope_id,
            subscription_key,
            name,
            source_type,
            source_key,
            target,
            enabled,
        }
    }
}

impl From<lash_core::facade_support::TriggerRegistration> for RemoteTriggerRegistration {
    #[expect(
        clippy::expect_used,
        reason = "RemoteProcessInput::try_from only errs when serde_json cannot serialize a crate-owned value; trigger process inputs serialize by construction"
    )]
    fn from(value: lash_core::facade_support::TriggerRegistration) -> Self {
        let lash_core::facade_support::TriggerRegistration {
            subscription_key,
            incarnation,
            revision,
            registrant,
            source_key,
            name,
            source_type,
            source,
            target,
            enabled,
        } = value;
        let lash_core::facade_support::TriggerTarget {
            label,
            identity,
            input,
            inputs,
        } = target;
        Self {
            subscription_key,
            incarnation,
            revision,
            registrant: registrant.into(),
            source_key,
            name,
            source_type: source_type.to_string(),
            source,
            target: RemoteTriggerTarget {
                label,
                identity: identity.into(),
                input: input
                    .try_into()
                    .expect("core process input serializes remotely"),
                inputs: inputs.into(),
            },
            enabled,
        }
    }
}

impl TryFrom<RemoteTriggerRegistration> for lash_core::facade_support::TriggerRegistration {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerRegistration) -> Result<Self, Self::Error> {
        value.validate("RemoteTriggerRegistration")?;
        let RemoteTriggerRegistration {
            subscription_key,
            incarnation,
            revision,
            registrant,
            source_key,
            name,
            source_type,
            source,
            target,
            enabled,
        } = value;
        let RemoteTriggerTarget {
            label,
            identity,
            input,
            inputs,
        } = target;
        Ok(Self {
            subscription_key,
            incarnation,
            revision,
            registrant: registrant.try_into()?,
            source_key,
            name,
            source_type: lash_core::facade_support::TriggerEventType::new(source_type),
            source,
            target: lash_core::facade_support::TriggerTarget {
                label,
                identity: identity.into(),
                input: input.try_into()?,
                inputs: inputs.into(),
            },
            enabled,
        })
    }
}

impl From<std::collections::BTreeMap<String, lash_core::TriggerInputBinding>>
    for RemoteTriggerInputTemplate
{
    fn from(value: std::collections::BTreeMap<String, lash_core::TriggerInputBinding>) -> Self {
        let entries = value
            .iter()
            .map(|(name, binding)| (name.to_string(), RemoteTriggerInputBinding::from(binding)))
            .collect();
        Self { entries }
    }
}

impl From<RemoteTriggerInputTemplate>
    for std::collections::BTreeMap<String, lash_core::TriggerInputBinding>
{
    fn from(value: RemoteTriggerInputTemplate) -> Self {
        let RemoteTriggerInputTemplate { entries } = value;
        entries
            .into_iter()
            .map(|(name, binding)| (name, binding.into()))
            .collect()
    }
}

impl From<&lash_core::TriggerInputBinding> for RemoteTriggerInputBinding {
    fn from(value: &lash_core::TriggerInputBinding) -> Self {
        match value {
            lash_core::TriggerInputBinding::Event => Self::Event,
            lash_core::TriggerInputBinding::Fixed { value } => Self::Fixed {
                value: value.clone(),
            },
        }
    }
}

impl From<RemoteTriggerInputBinding> for lash_core::TriggerInputBinding {
    fn from(value: RemoteTriggerInputBinding) -> Self {
        match value {
            RemoteTriggerInputBinding::Event => Self::Event,
            RemoteTriggerInputBinding::Fixed { value } => Self::Fixed { value },
        }
    }
}

impl From<lash_core::TriggerOwnerScope> for RemoteTriggerOwnerScope {
    fn from(value: lash_core::TriggerOwnerScope) -> Self {
        match value {
            lash_core::TriggerOwnerScope::Session { session_id } => Self::Session { session_id },
            lash_core::TriggerOwnerScope::Host { binding_id } => Self::Host { binding_id },
            lash_core::TriggerOwnerScope::Platform => Self::Platform,
        }
    }
}

impl TryFrom<RemoteTriggerOwnerScope> for lash_core::TriggerOwnerScope {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerOwnerScope) -> Result<Self, Self::Error> {
        value.validate("RemoteTriggerOwnerScope")?;
        Ok(match value {
            RemoteTriggerOwnerScope::Session { session_id } => Self::Session { session_id },
            RemoteTriggerOwnerScope::Host { binding_id } => Self::Host { binding_id },
            RemoteTriggerOwnerScope::Platform => Self::Platform,
        })
    }
}

impl From<RemoteTriggerSourceCapture> for lash_core::TriggerSourceCapture {
    fn from(value: RemoteTriggerSourceCapture) -> Self {
        let RemoteTriggerSourceCapture {
            constructor_path,
            config_schema,
            route,
        } = value;
        Self {
            constructor_path,
            config_schema: lash_core::LashSchema::new(config_schema),
            route: route.into(),
        }
    }
}

impl From<lash_core::TriggerSourceCapture> for RemoteTriggerSourceCapture {
    fn from(value: lash_core::TriggerSourceCapture) -> Self {
        let lash_core::TriggerSourceCapture {
            constructor_path,
            config_schema,
            route,
        } = value;
        Self {
            constructor_path,
            config_schema: config_schema.schema,
            route: route.into(),
        }
    }
}

impl From<RemoteTriggerProviderRoute> for lash_core::TriggerProviderRoute {
    fn from(value: RemoteTriggerProviderRoute) -> Self {
        match value {
            RemoteTriggerProviderRoute::Resident => Self::Resident,
            RemoteTriggerProviderRoute::Provider { provider_id, route } => {
                Self::Provider { provider_id, route }
            }
        }
    }
}

impl From<lash_core::TriggerProviderRoute> for RemoteTriggerProviderRoute {
    fn from(value: lash_core::TriggerProviderRoute) -> Self {
        match value {
            lash_core::TriggerProviderRoute::Resident => Self::Resident,
            lash_core::TriggerProviderRoute::Provider { provider_id, route } => {
                Self::Provider { provider_id, route }
            }
        }
    }
}

impl TryFrom<RemoteTriggerSubscriptionDraft> for lash_core::TriggerSubscriptionDraft {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerSubscriptionDraft) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteTriggerSubscriptionSpec {
            subscription_key,
            env_ref,
            wake_target,
            name,
            source_type,
            source_key,
            source,
            payload_schema,
            source_capture,
            target,
            target_identity,
            event_types,
            input_template,
            target_label,
        } = value.spec;
        Ok(Self {
            subscription_key,
            env_ref: lash_core::ProcessExecutionEnvRef::new(env_ref.as_str().to_string()),
            wake_target: wake_target.map(TryInto::try_into).transpose()?,
            name,
            source_type,
            source_key,
            source,
            payload_schema: lash_core::LashSchema::new(payload_schema),
            source_capture: source_capture.into(),
            target: target.try_into()?,
            target_identity: target_identity.into(),
            event_types: event_types.into_iter().map(Into::into).collect(),
            input_template: input_template.into(),
            target_label,
        })
    }
}

impl TryFrom<lash_core::TriggerSubscriptionDraft> for RemoteTriggerSubscriptionDraft {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::TriggerSubscriptionDraft) -> Result<Self, Self::Error> {
        let lash_core::TriggerSubscriptionDraft {
            subscription_key,
            env_ref,
            wake_target,
            name,
            source_type,
            source_key,
            source,
            payload_schema,
            source_capture,
            target,
            target_identity,
            event_types,
            input_template,
            target_label,
        } = value;
        Ok(Self {
            spec: RemoteTriggerSubscriptionSpec {
                subscription_key,
                env_ref: env_ref.as_str().parse()?,
                wake_target: wake_target.map(Into::into),
                name,
                source_type,
                source_key,
                source,
                payload_schema: payload_schema.schema,
                source_capture: source_capture.into(),
                target: target.try_into()?,
                target_identity: target_identity.into(),
                event_types: event_types.into_iter().map(Into::into).collect(),
                input_template: input_template.into(),
                target_label,
            },
        })
    }
}

impl TryFrom<lash_core::TriggerSubscriptionRecord> for RemoteTriggerSubscriptionRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::TriggerSubscriptionRecord) -> Result<Self, Self::Error> {
        let lash_core::TriggerSubscriptionRecord {
            subscription_id,
            owner_scope,
            subscription_key,
            incarnation,
            revision,
            definition_fingerprint,
            registrant,
            env_ref,
            wake_target,
            name,
            source_type,
            source_key,
            source,
            payload_schema,
            source_capture,
            target,
            target_identity,
            event_types,
            input_template,
            target_label,
            lifecycle,
            created_at_ms,
            updated_at_ms,
        } = value;
        Ok(Self {
            subscription_id,
            owner_scope: owner_scope.into(),
            incarnation,
            revision,
            definition_fingerprint,
            registrant: registrant.into(),
            spec: RemoteTriggerSubscriptionSpec {
                subscription_key,
                env_ref: env_ref.as_str().parse()?,
                wake_target: wake_target.map(Into::into),
                name,
                source_type,
                source_key,
                source,
                payload_schema: payload_schema.schema,
                source_capture: source_capture.into(),
                target: target.try_into()?,
                target_identity: target_identity.into(),
                event_types: event_types.into_iter().map(Into::into).collect(),
                input_template: input_template.into(),
                target_label,
            },
            lifecycle: lifecycle.into(),
            created_at_ms,
            updated_at_ms,
        })
    }
}

impl TryFrom<RemoteTriggerSubscriptionRecord> for lash_core::TriggerSubscriptionRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerSubscriptionRecord) -> Result<Self, Self::Error> {
        value.validate("RemoteTriggerSubscriptionRecord")?;
        let RemoteTriggerSubscriptionRecord {
            subscription_id,
            owner_scope,
            incarnation,
            revision,
            definition_fingerprint,
            registrant,
            spec,
            lifecycle,
            created_at_ms,
            updated_at_ms,
        } = value;
        let RemoteTriggerSubscriptionSpec {
            subscription_key,
            env_ref,
            wake_target,
            name,
            source_type,
            source_key,
            source,
            payload_schema,
            source_capture,
            target,
            target_identity,
            event_types,
            input_template,
            target_label,
        } = spec;
        Ok(Self {
            subscription_id,
            owner_scope: owner_scope.try_into()?,
            subscription_key,
            incarnation,
            revision,
            definition_fingerprint,
            registrant: registrant.try_into()?,
            env_ref: lash_core::ProcessExecutionEnvRef::new(env_ref.as_str().to_string()),
            wake_target: wake_target.map(TryInto::try_into).transpose()?,
            name,
            source_type,
            source_key,
            source,
            payload_schema: lash_core::LashSchema::new(payload_schema),
            source_capture: source_capture.into(),
            target: target.try_into()?,
            target_identity: target_identity.into(),
            event_types: event_types.into_iter().map(Into::into).collect(),
            input_template: input_template.into(),
            target_label,
            lifecycle: lifecycle.into(),
            created_at_ms,
            updated_at_ms,
        })
    }
}

impl TryFrom<RemoteTriggerRegisterSubscriptionRequest> for lash_core::TriggerSubscriptionDraft {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerRegisterSubscriptionRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteTriggerRegisterSubscriptionRequest { draft } = value;
        draft.try_into()
    }
}

impl TryFrom<lash_core::TriggerSubscriptionRecord> for RemoteTriggerRegisterSubscriptionReceipt {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::TriggerSubscriptionRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            record: value.try_into()?,
        })
    }
}

impl TryFrom<RemoteTriggerRegisterSubscriptionReceipt> for lash_core::TriggerSubscriptionRecord {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerRegisterSubscriptionReceipt) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteTriggerRegisterSubscriptionReceipt { record } = value;
        record.try_into()
    }
}

impl TryFrom<Vec<lash_core::TriggerSubscriptionRecord>> for RemoteTriggerListSubscriptionsResponse {
    type Error = RemoteProtocolError;

    fn try_from(value: Vec<lash_core::TriggerSubscriptionRecord>) -> Result<Self, Self::Error> {
        Ok(Self {
            subscriptions: value
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

impl TryFrom<RemoteTriggerListSubscriptionsResponse> for Vec<lash_core::TriggerSubscriptionRecord> {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTriggerListSubscriptionsResponse) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteTriggerListSubscriptionsResponse { subscriptions } = value;
        subscriptions.into_iter().map(TryInto::try_into).collect()
    }
}

pub(crate) fn decode_remote_json<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    type_name: &'static str,
    field: &'static str,
) -> Result<T, RemoteProtocolError> {
    serde_json::from_value(value).map_err(|err| RemoteProtocolError::InvalidEnvelope {
        type_name,
        message: format!("invalid {field}: {err}"),
    })
}

pub(crate) fn encode_remote_json<T: serde::Serialize>(
    value: T,
    type_name: &'static str,
    field: &'static str,
) -> Result<serde_json::Value, RemoteProtocolError> {
    serde_json::to_value(value).map_err(|err| RemoteProtocolError::InvalidEnvelope {
        type_name,
        message: format!("cannot encode {field}: {err}"),
    })
}
