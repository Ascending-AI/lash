//! Trigger envelopes: occurrence emission, subscriptions, and registrations.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::processes::{
    RemoteProcessDefinitionIdentity, RemoteProcessEventType, RemoteProcessExecutionEnvRef,
    RemoteProcessIdentity, RemoteProcessInput, RemoteProcessOriginator, RemoteSessionScope,
};
use crate::registry_errors::{RemoteProtocolError, require_non_empty};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteTriggerOccurrenceOutcome {
    #[default]
    Fired,
    CoalescedInto {
        occurrence_id: String,
    },
    Dropped {
        reason: String,
    },
}

impl RemoteTriggerOccurrenceOutcome {
    fn is_fired(&self) -> bool {
        matches!(self, Self::Fired)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerOccurrenceRequest {
    pub source_type: String,
    pub source_key: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "RemoteTriggerOccurrenceOutcome::is_fired"
    )]
    pub outcome: RemoteTriggerOccurrenceOutcome,
}

impl RemoteTriggerOccurrenceRequest {
    pub fn new(
        source_type: impl Into<String>,
        source_key: impl Into<String>,
        payload: serde_json::Value,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            source_type: source_type.into(),
            source_key: source_key.into(),
            payload,
            idempotency_key: idempotency_key.into(),
            source: None,
            session_id: None,
            outcome: RemoteTriggerOccurrenceOutcome::Fired,
        }
    }

    pub fn with_source(mut self, source: serde_json::Value) -> Self {
        self.source = Some(source);
        self
    }

    pub fn for_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_outcome(mut self, outcome: RemoteTriggerOccurrenceOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty(
            "RemoteTriggerOccurrenceRequest",
            "source_type",
            &self.source_type,
        )?;
        require_non_empty(
            "RemoteTriggerOccurrenceRequest",
            "source_key",
            &self.source_key,
        )?;
        require_non_empty(
            "RemoteTriggerOccurrenceRequest",
            "idempotency_key",
            &self.idempotency_key,
        )?;
        if let Some(session_id) = &self.session_id {
            require_non_empty("RemoteTriggerOccurrenceRequest", "session_id", session_id)?;
        }
        match &self.outcome {
            RemoteTriggerOccurrenceOutcome::Fired => {}
            RemoteTriggerOccurrenceOutcome::CoalescedInto { occurrence_id } => {
                require_non_empty(
                    "RemoteTriggerOccurrenceRequest",
                    "outcome.occurrence_id",
                    occurrence_id,
                )?;
            }
            RemoteTriggerOccurrenceOutcome::Dropped { reason } => {
                require_non_empty("RemoteTriggerOccurrenceRequest", "outcome.reason", reason)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerOccurrenceRecord {
    pub occurrence_id: String,
    pub source_type: String,
    pub source_key: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "RemoteTriggerOccurrenceOutcome::is_fired"
    )]
    pub outcome: RemoteTriggerOccurrenceOutcome,
    pub occurred_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTriggerDeliveryEmitOutcome {
    Started,
    AlreadyReserved,
    Failed { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerDeliveryEmitReceipt {
    pub occurrence_id: String,
    pub subscription_id: String,
    pub process_id: String,
    pub outcome: RemoteTriggerDeliveryEmitOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerEmitReport {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub occurrence_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<RemoteTriggerDeliveryEmitReceipt>,
}

impl RemoteTriggerEmitReport {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        // The envelope owns validation; this report has no remaining local invariants.
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerSubscriptionFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registrant_scope_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<RemoteProcessDefinitionIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl RemoteTriggerSubscriptionFilter {
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            ..Self::default()
        }
    }

    pub fn for_registrant_scope(scope_id: impl Into<String>) -> Self {
        Self {
            registrant_scope_id: Some(scope_id.into()),
            ..Self::default()
        }
    }

    pub fn for_source_type(source_type: impl Into<String>) -> Self {
        Self {
            source_type: Some(source_type.into()),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        // The envelope owns validation; this filter has no remaining local invariants.
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerRegistration {
    pub subscription_key: String,
    pub incarnation: String,
    pub revision: u64,
    pub registrant: RemoteProcessOriginator,
    pub manifest_membership: RemoteTriggerManifestMembership,
    pub source_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source_type: String,
    #[serde(default)]
    pub source: serde_json::Value,
    pub target: RemoteTriggerTarget,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTriggerManifestMembership {
    PresentInCurrentArtifact,
    Orphaned,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub identity: RemoteProcessIdentity,
    pub input: RemoteProcessInput,
    #[serde(default)]
    pub inputs: RemoteTriggerInputTemplate,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RemoteTriggerInputTemplate {
    pub entries: BTreeMap<String, RemoteTriggerInputBinding>,
}

impl RemoteTriggerInputTemplate {
    pub fn new(entries: BTreeMap<String, RemoteTriggerInputBinding>) -> Self {
        Self { entries }
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        for name in self.entries.keys() {
            require_non_empty(type_name, "input_template key", name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteTriggerInputBinding {
    Event,
    Fixed { value: serde_json::Value },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTriggerOwnerScope {
    Session { session_id: String },
    Host { binding_id: String },
    Platform,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerSubscriptionDraft {
    pub subscription_key: String,
    pub env_ref: RemoteProcessExecutionEnvRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_target: Option<RemoteSessionScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source_type: String,
    pub source_key: String,
    #[serde(default)]
    pub source: serde_json::Value,
    #[serde(default)]
    pub payload_schema: serde_json::Value,
    pub target: RemoteProcessInput,
    pub target_identity: RemoteProcessIdentity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_types: Vec<RemoteProcessEventType>,
    #[serde(default)]
    pub input_template: RemoteTriggerInputTemplate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
}

impl RemoteTriggerSubscriptionDraft {
    pub fn for_process(
        subscription_key: impl Into<String>,
        env_ref: RemoteProcessExecutionEnvRef,
        source_type: impl Into<String>,
        source_key: impl Into<String>,
        target: RemoteProcessInput,
        target_identity: RemoteProcessIdentity,
    ) -> Self {
        let target_label = target_identity.label.clone();
        Self {
            subscription_key: subscription_key.into(),
            env_ref,
            wake_target: None,
            name: None,
            source_type: source_type.into(),
            source_key: source_key.into(),
            source: serde_json::Value::Object(serde_json::Map::new()),
            payload_schema: serde_json::Value::Object(serde_json::Map::new()),
            target,
            target_identity,
            event_types: Vec::new(),
            input_template: RemoteTriggerInputTemplate::default(),
            target_label,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_source(mut self, source: serde_json::Value) -> Self {
        self.source = source;
        self
    }

    pub fn with_payload_schema(mut self, payload_schema: serde_json::Value) -> Self {
        self.payload_schema = payload_schema;
        self
    }

    pub fn with_wake_target(mut self, wake_target: RemoteSessionScope) -> Self {
        self.wake_target = Some(wake_target);
        self
    }

    pub fn with_event_types(
        mut self,
        event_types: impl IntoIterator<Item = RemoteProcessEventType>,
    ) -> Self {
        self.event_types = event_types.into_iter().collect();
        self
    }

    pub fn with_input_template(mut self, input_template: RemoteTriggerInputTemplate) -> Self {
        self.input_template = input_template;
        self
    }

    pub fn with_target_label(mut self, target_label: impl Into<String>) -> Self {
        self.target_label = Some(target_label.into());
        self
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty(
            "RemoteTriggerSubscriptionDraft",
            "subscription_key",
            &self.subscription_key,
        )?;
        self.env_ref.validate("RemoteTriggerSubscriptionDraft")?;
        if let Some(wake_target) = &self.wake_target {
            wake_target.validate("RemoteTriggerSubscriptionDraft")?;
        }
        require_non_empty(
            "RemoteTriggerSubscriptionDraft",
            "source_type",
            &self.source_type,
        )?;
        require_non_empty(
            "RemoteTriggerSubscriptionDraft",
            "source_key",
            &self.source_key,
        )?;
        self.target.validate("RemoteTriggerSubscriptionDraft")?;
        self.target_identity
            .validate("RemoteTriggerSubscriptionDraft")?;
        for event_type in &self.event_types {
            event_type.validate("RemoteTriggerSubscriptionDraft")?;
        }
        validate_remote_trigger_target_label(
            "RemoteTriggerSubscriptionDraft",
            self.target_label.as_deref(),
            self.target_identity.label.as_deref(),
        )?;
        self.input_template
            .validate("RemoteTriggerSubscriptionDraft")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerSubscriptionRecord {
    pub subscription_id: String,
    pub owner_scope: RemoteTriggerOwnerScope,
    pub subscription_key: String,
    pub incarnation: String,
    pub revision: u64,
    pub definition_fingerprint: String,
    pub registrant: RemoteProcessOriginator,
    pub env_ref: RemoteProcessExecutionEnvRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_target: Option<RemoteSessionScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source_type: String,
    pub source_key: String,
    #[serde(default)]
    pub source: serde_json::Value,
    #[serde(default)]
    pub payload_schema: serde_json::Value,
    pub target: RemoteProcessInput,
    pub target_identity: RemoteProcessIdentity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_types: Vec<RemoteProcessEventType>,
    #[serde(default)]
    pub input_template: RemoteTriggerInputTemplate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tombstoned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl RemoteTriggerSubscriptionRecord {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "subscription_id", &self.subscription_id)?;
        require_non_empty(type_name, "subscription_key", &self.subscription_key)?;
        require_non_empty(type_name, "incarnation", &self.incarnation)?;
        require_non_empty(
            type_name,
            "definition_fingerprint",
            &self.definition_fingerprint,
        )?;
        self.registrant.validate(type_name)?;
        self.env_ref.validate(type_name)?;
        if let Some(wake_target) = &self.wake_target {
            wake_target.validate(type_name)?;
        }
        require_non_empty(type_name, "source_type", &self.source_type)?;
        require_non_empty(type_name, "source_key", &self.source_key)?;
        self.target.validate(type_name)?;
        self.target_identity.validate(type_name)?;
        for event_type in &self.event_types {
            event_type.validate(type_name)?;
        }
        validate_remote_trigger_target_label(
            type_name,
            self.target_label.as_deref(),
            self.target_identity.label.as_deref(),
        )?;
        self.input_template.validate(type_name)
    }
}

fn validate_remote_trigger_target_label(
    type_name: &'static str,
    target_label: Option<&str>,
    identity_label: Option<&str>,
) -> Result<(), RemoteProtocolError> {
    match (target_label, identity_label) {
        (Some(target_label), Some(identity_label)) if target_label != identity_label => {
            Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "target_label must match target_identity.label when both are present"
                    .to_string(),
            })
        }
        _ => Ok(()),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerRegisterSubscriptionRequest {
    pub draft: RemoteTriggerSubscriptionDraft,
}

impl RemoteTriggerRegisterSubscriptionRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.draft.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerRegisterSubscriptionReceipt {
    pub record: RemoteTriggerSubscriptionRecord,
}

impl RemoteTriggerRegisterSubscriptionReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.record
            .validate("RemoteTriggerRegisterSubscriptionReceipt")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTriggerListSubscriptionsResponse {
    #[serde(default)]
    pub subscriptions: Vec<RemoteTriggerSubscriptionRecord>,
}

impl RemoteTriggerListSubscriptionsResponse {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        for record in &self.subscriptions {
            record.validate("RemoteTriggerListSubscriptionsResponse")?;
        }
        Ok(())
    }
}
