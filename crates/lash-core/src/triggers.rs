use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::plugin::PluginError;

mod memory;
mod mutation;
mod router;

pub use memory::InMemoryTriggerStore;
#[cfg(any(test, feature = "testing"))]
pub use memory::RawTriggerStateForTesting;
use memory::{
    InMemoryTriggerDeliveryRecord, InMemoryTriggerEventState, apply_in_memory_trigger_command,
};
pub use mutation::evaluate_trigger_mutation;
use mutation::{
    ensure_live_revision, mutate_enabled, subscription_conflict, subscription_record_from_draft,
};
pub use router::*;
use router::{default_enabled, reserve_in_memory_for_occurrence};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerEvent {
    pub resource_type: String,
    pub alias: String,
    pub event: String,
    pub payload_schema: crate::LashSchema,
}

impl TriggerEvent {
    pub fn new(
        resource_type: impl Into<String>,
        alias: impl Into<String>,
        event: impl Into<String>,
        payload_schema: crate::LashSchema,
    ) -> Self {
        Self {
            resource_type: resource_type.into(),
            alias: alias.into(),
            event: event.into(),
            payload_schema,
        }
    }

    pub fn payload_schema(&self) -> &crate::LashSchema {
        &self.payload_schema
    }

    pub fn key(&self) -> TriggerEventKey {
        TriggerEventKey {
            resource_type: self.resource_type.clone(),
            alias: self.alias.clone(),
            event: self.event.clone(),
        }
    }

    pub fn source_type(&self) -> String {
        trigger_event_type(&self.alias, &self.event)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TriggerEventKey {
    pub resource_type: String,
    pub alias: String,
    pub event: String,
}

impl TriggerEventKey {
    pub fn new(
        resource_type: impl Into<String>,
        alias: impl Into<String>,
        event: impl Into<String>,
    ) -> Self {
        Self {
            resource_type: resource_type.into(),
            alias: alias.into(),
            event: event.into(),
        }
    }

    pub fn source_type(&self) -> String {
        trigger_event_type(&self.alias, &self.event)
    }
}

pub fn trigger_event_type(alias: &str, event: &str) -> String {
    format!("{alias}.{event}")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerEventCatalog {
    events: BTreeMap<TriggerEventKey, TriggerEvent>,
}

impl TriggerEventCatalog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn declare(&mut self, event: TriggerEvent) -> Result<(), String> {
        let key = event.key();
        if self.events.contains_key(&key) {
            return Err(format!(
                "duplicate trigger occurrence `{}.{}.{}`",
                key.resource_type, key.alias, key.event
            ));
        }
        let source_type = event.source_type();
        if let Some(existing) = self
            .events
            .values()
            .find(|existing| existing.source_type() == source_type)
        {
            return Err(format!(
                "duplicate trigger source `{source_type}` declared by `{}.{}.{}` and `{}.{}.{}`",
                existing.resource_type,
                existing.alias,
                existing.event,
                key.resource_type,
                key.alias,
                key.event
            ));
        }
        self.events.insert(key, event);
        Ok(())
    }

    pub(crate) fn from_events(
        events: impl IntoIterator<Item = TriggerEvent>,
    ) -> Result<Self, String> {
        let mut catalog = Self::new();
        for event in events {
            catalog.declare(event)?;
        }
        Ok(catalog)
    }

    #[cfg(test)]
    pub(crate) fn get(
        &self,
        resource_type: &str,
        alias: &str,
        event: &str,
    ) -> Option<&TriggerEvent> {
        self.events
            .get(&TriggerEventKey::new(resource_type, alias, event))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerDeliveryEmitOutcome {
    Started,
    AlreadyReserved,
    Failed { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerDeliveryEmitReport {
    pub occurrence_id: String,
    pub subscription_id: String,
    pub process_id: String,
    pub outcome: TriggerDeliveryEmitOutcome,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerEmitReport {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub occurrence_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<TriggerDeliveryEmitReport>,
}

impl TriggerEmitReport {
    pub fn empty() -> Self {
        Self::default()
    }

    fn new(occurrence_id: String, deliveries: Vec<TriggerDeliveryEmitReport>) -> Self {
        Self {
            occurrence_id,
            deliveries,
        }
    }

    pub fn started_process_ids(&self) -> Vec<String> {
        self.deliveries
            .iter()
            .filter(|delivery| delivery.outcome == TriggerDeliveryEmitOutcome::Started)
            .map(|delivery| delivery.process_id.clone())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerOccurrenceRequest {
    pub source_type: String,
    pub source_key: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
    /// Optional host routing scope. When present, only subscriptions
    /// registered by this session can reserve deliveries for the occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl TriggerOccurrenceRequest {
    /// Constructs an occurrence for trigger-store implementors with explicit source identity and
    /// caller-supplied idempotency key; host routing remains unrestricted until scoped.
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
        }
    }

    /// Sets the source carried by a `TriggerOccurrenceRequest` for store and durable-substrate
    /// implementors while persisting trigger subscriptions and occurrences.
    pub fn with_source(mut self, source: serde_json::Value) -> Self {
        self.source = Some(source);
        self
    }

    /// Restricts occurrence delivery reservation to subscriptions registered by one session for
    /// trigger-store implementors enforcing host routing scope.
    pub fn for_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerOccurrenceRecord {
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
    pub occurred_at_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerOccurrenceFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at_start_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at_end_ms: Option<u64>,
}

impl TriggerOccurrenceFilter {
    /// Constructs a `TriggerOccurrenceFilter` using for source semantics for store and
    /// durable-substrate implementors while persisting trigger subscriptions and occurrences.
    pub fn for_source(source_type: impl Into<String>, source_key: impl Into<String>) -> Self {
        Self {
            source_type: Some(source_type.into()),
            source_key: Some(source_key.into()),
            ..Self::default()
        }
    }

    /// Applies every populated occurrence filter conjunctively for trigger-store and conformance
    /// implementors; occurrence time uses a half-open `[start, end)` range.
    pub fn matches(&self, record: &TriggerOccurrenceRecord) -> bool {
        self.source_type
            .as_deref()
            .is_none_or(|source_type| record.source_type == source_type)
            && self
                .source_key
                .as_deref()
                .is_none_or(|source_key| record.source_key == source_key)
            && self
                .occurred_at_start_ms
                .is_none_or(|start_ms| record.occurred_at_ms >= start_ms)
            && self
                .occurred_at_end_ms
                .is_none_or(|end_ms| record.occurred_at_ms < end_ms)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TriggerEventType(String);

impl TriggerEventType {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TriggerEventType {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for TriggerEventType {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl AsRef<str> for TriggerEventType {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for TriggerEventType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerRegistration {
    pub subscription_key: String,
    pub incarnation: String,
    pub revision: u64,
    pub registrant: crate::ProcessOriginator,
    pub manifest_membership: TriggerManifestMembership,
    pub source_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source_type: TriggerEventType,
    pub source: serde_json::Value,
    pub target: TriggerTargetSummary,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerManifestMembership {
    PresentInCurrentArtifact,
    Orphaned,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerTargetSummary {
    pub label: Option<String>,
    pub identity: crate::ProcessIdentity,
    pub input: crate::ProcessInput,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, TriggerInputBinding>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerInputBinding {
    Event,
    Fixed { value: serde_json::Value },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerSubscriptionDraft {
    pub subscription_key: String,
    pub env_ref: crate::ProcessExecutionEnvRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_target: Option<crate::SessionScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source_type: String,
    pub source_key: String,
    pub source: serde_json::Value,
    pub payload_schema: crate::LashSchema,
    pub target: crate::ProcessInput,
    pub target_identity: crate::ProcessIdentity,
    #[serde(default)]
    pub event_types: Vec<crate::ProcessEventType>,
    #[serde(default)]
    pub input_template: BTreeMap<String, TriggerInputBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
}

impl TriggerSubscriptionDraft {
    /// Constructs process-targeted subscription state for trigger-store and process-engine
    /// implementors with empty source metadata, schema, event types, and bindings, while inheriting
    /// the identity label.
    pub fn for_process(
        subscription_key: impl Into<String>,
        env_ref: crate::ProcessExecutionEnvRef,
        source_type: impl Into<String>,
        source_key: impl Into<String>,
        target: crate::ProcessInput,
        target_identity: crate::ProcessIdentity,
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
            payload_schema: crate::LashSchema::new(serde_json::Value::Object(
                serde_json::Map::new(),
            )),
            target,
            target_identity,
            event_types: Vec::new(),
            input_template: BTreeMap::new(),
            target_label,
        }
    }

    /// Sets the name carried by a `TriggerSubscriptionDraft` for store and process-engine
    /// implementors while persisting trigger subscriptions, occurrences, and deliveries.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the source carried by a `TriggerSubscriptionDraft` for store and process-engine
    /// implementors while persisting trigger subscriptions, occurrences, and deliveries.
    pub fn with_source(mut self, source: serde_json::Value) -> Self {
        self.source = source;
        self
    }

    /// Sets the payload schema carried by a `TriggerSubscriptionDraft` for store and process-engine
    /// implementors while persisting trigger subscriptions, occurrences, and deliveries.
    pub fn with_payload_schema(mut self, payload_schema: crate::LashSchema) -> Self {
        self.payload_schema = payload_schema;
        self
    }

    /// Sets the wake target carried by a `TriggerSubscriptionDraft` for store and process-engine
    /// implementors while persisting trigger subscriptions, occurrences, and deliveries.
    pub fn with_wake_target(mut self, wake_target: crate::SessionScope) -> Self {
        self.wake_target = Some(wake_target);
        self
    }

    /// Sets the event types carried by a `TriggerSubscriptionDraft` for store and process-engine
    /// implementors while persisting trigger subscriptions, occurrences, and deliveries.
    pub fn with_event_types(
        mut self,
        event_types: impl IntoIterator<Item = crate::ProcessEventType>,
    ) -> Self {
        self.event_types = event_types.into_iter().collect();
        self
    }

    /// Sets the input template carried by a `TriggerSubscriptionDraft` for store and process-engine
    /// implementors while persisting trigger subscriptions, occurrences, and deliveries.
    pub fn with_input_template(
        mut self,
        input_template: BTreeMap<String, TriggerInputBinding>,
    ) -> Self {
        self.input_template = input_template;
        self
    }

    /// Sets the target label carried by a `TriggerSubscriptionDraft` for store and process-engine
    /// implementors while persisting trigger subscriptions, occurrences, and deliveries.
    pub fn with_target_label(mut self, target_label: impl Into<String>) -> Self {
        self.target_label = Some(target_label.into());
        self
    }

    /// Rejects an empty or reserved subscription key and any target label that disagrees with the
    /// process identity label before trigger-store implementors persist the draft.
    pub fn validate(&self) -> Result<(), PluginError> {
        validate_subscription_key(&self.subscription_key, false)?;
        validate_trigger_subscription_target_label(
            self.target_label.as_deref(),
            self.target_identity.label.as_deref(),
        )
    }
}

pub const INTERNAL_TRIGGER_KEY_PREFIX: &str = "lash.internal/";

pub fn validate_subscription_key(key: &str, internal: bool) -> Result<(), PluginError> {
    if key.trim().is_empty() {
        return Err(PluginError::Session(
            "trigger subscription requires subscription_key".to_string(),
        ));
    }
    if !internal && key.starts_with(INTERNAL_TRIGGER_KEY_PREFIX) {
        return Err(PluginError::Session(format!(
            "trigger subscription key `{key}` uses reserved prefix `{INTERNAL_TRIGGER_KEY_PREFIX}`"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerOwnerScope {
    Session { session_id: String },
    Host { binding_id: String },
    Platform,
}

impl TriggerOwnerScope {
    /// Constructs a `TriggerOwnerScope` using session semantics for store and process-engine
    /// implementors while persisting trigger subscriptions, occurrences, and deliveries.
    pub fn session(session_id: impl Into<String>) -> Self {
        Self::Session {
            session_id: session_id.into(),
        }
    }

    /// Constructs host-owned trigger scope for store implementors and rejects an empty binding ID
    /// so its durable namespace cannot collapse.
    pub fn host(binding_id: impl Into<String>) -> Result<Self, PluginError> {
        let binding_id = binding_id.into();
        if binding_id.trim().is_empty() {
            return Err(PluginError::Session(
                "trigger host owner requires a non-empty binding id".to_string(),
            ));
        }
        Ok(Self::Host { binding_id })
    }

    /// Derives the stable owner namespace trigger-store implementors use for isolation:
    /// `session:<id>`, `host:<binding>`, or the reserved platform `host` namespace.
    pub fn namespace(&self) -> String {
        match self {
            Self::Session { session_id } => format!("session:{session_id}"),
            Self::Host { binding_id } => format!("host:{binding_id}"),
            Self::Platform => "host".to_string(),
        }
    }

    /// Exposes the owning session to trigger-store implementors only for session scope, returning
    /// `None` for host and platform ownership.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Session { session_id } => Some(session_id),
            Self::Host { .. } | Self::Platform => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerSubscriptionRecord {
    pub subscription_id: String,
    pub owner_scope: TriggerOwnerScope,
    pub subscription_key: String,
    pub incarnation: String,
    pub revision: u64,
    pub definition_hash: String,
    pub registrant: crate::ProcessOriginator,
    pub env_ref: crate::ProcessExecutionEnvRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_target: Option<crate::SessionScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source_type: String,
    pub source_key: String,
    pub source: serde_json::Value,
    pub payload_schema: crate::LashSchema,
    pub target: crate::ProcessInput,
    pub target_identity: crate::ProcessIdentity,
    #[serde(default)]
    pub event_types: Vec<crate::ProcessEventType>,
    #[serde(default)]
    pub input_template: BTreeMap<String, TriggerInputBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub tombstoned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl TriggerSubscriptionRecord {
    /// Projects the canonical owner namespace for trigger-store implementors filtering records
    /// across session, host, and platform registrants.
    pub fn registrant_scope_id(&self) -> String {
        self.owner_scope.namespace()
    }

    /// Exposes the registrant session to trigger-store implementors only for session-owned records,
    /// returning `None` for host and platform ownership.
    pub fn registrant_session_id(&self) -> Option<&str> {
        self.owner_scope.session_id()
    }
}

fn validate_trigger_subscription_target_label(
    target_label: Option<&str>,
    identity_label: Option<&str>,
) -> Result<(), PluginError> {
    match (target_label, identity_label) {
        (Some(target_label), Some(identity_label)) if target_label != identity_label => {
            Err(PluginError::Session(
                "trigger target_label must match target_identity.label when both are present"
                    .to_string(),
            ))
        }
        _ => Ok(()),
    }
}

impl From<&TriggerSubscriptionRecord> for TriggerRegistration {
    fn from(route: &TriggerSubscriptionRecord) -> Self {
        Self::from_record_with_manifest(route, None)
    }
}

impl TriggerRegistration {
    pub fn from_record_with_manifest(
        route: &TriggerSubscriptionRecord,
        manifest_keys: Option<&std::collections::BTreeSet<String>>,
    ) -> Self {
        Self {
            subscription_key: route.subscription_key.clone(),
            incarnation: route.incarnation.clone(),
            revision: route.revision,
            registrant: route.registrant.clone(),
            manifest_membership: manifest_keys.map_or(TriggerManifestMembership::Unknown, |keys| {
                if keys.contains(&route.subscription_key) {
                    TriggerManifestMembership::PresentInCurrentArtifact
                } else {
                    TriggerManifestMembership::Orphaned
                }
            }),
            source_key: route.source_key.clone(),
            name: route.name.clone(),
            source_type: TriggerEventType::new(route.source_type.clone()),
            source: route.source.clone(),
            target: TriggerTargetSummary {
                label: route.target_label.clone(),
                identity: route.target_identity.clone(),
                input: route.target.clone(),
                inputs: route.input_template.clone(),
            },
            enabled: route.enabled,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerSubscriptionFilter {
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
    pub target: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl TriggerSubscriptionFilter {
    /// Constructs a `TriggerSubscriptionFilter` using for session semantics for store and
    /// durable-substrate implementors while persisting trigger subscriptions and occurrences.
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            ..Self::default()
        }
    }

    /// Constructs a `TriggerSubscriptionFilter` using for registrant scope semantics for store and
    /// durable-substrate implementors while persisting trigger subscriptions and occurrences.
    pub fn for_registrant_scope(scope_id: impl Into<String>) -> Self {
        Self {
            registrant_scope_id: Some(scope_id.into()),
            ..Self::default()
        }
    }

    /// Constructs a `TriggerSubscriptionFilter` using for source type semantics for store and
    /// durable-substrate implementors while persisting trigger subscriptions and occurrences.
    pub fn for_source_type(source_type: impl Into<String>) -> Self {
        Self {
            source_type: Some(source_type.into()),
            ..Self::default()
        }
    }

    /// Returns only the explicit canonical registrant scope for trigger-store implementors; legacy
    /// session filtering remains a separate predicate.
    pub fn effective_registrant_scope_id(&self) -> Option<String> {
        self.registrant_scope_id.clone()
    }

    /// Applies every populated subscription filter conjunctively for trigger-store and conformance
    /// implementors and always excludes tombstoned records.
    pub fn matches(&self, record: &TriggerSubscriptionRecord) -> bool {
        self.effective_registrant_scope_id()
            .is_none_or(|scope_id| record.registrant_scope_id() == scope_id)
            && self
                .session_id
                .as_deref()
                .is_none_or(|session_id| record.registrant_session_id() == Some(session_id))
            && self
                .subscription_key
                .as_deref()
                .is_none_or(|key| record.subscription_key == key)
            && self
                .name
                .as_deref()
                .is_none_or(|name| record.name.as_deref() == Some(name))
            && self
                .source_type
                .as_deref()
                .is_none_or(|source_type| record.source_type == source_type)
            && self
                .source_key
                .as_deref()
                .is_none_or(|source_key| record.source_key == source_key)
            && self.enabled.is_none_or(|enabled| record.enabled == enabled)
            && !record.tombstoned
            && self
                .target
                .as_ref()
                .is_none_or(|target| record.target_identity.definition.as_ref() == Some(target))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerMutationDisposition {
    Created,
    Unchanged,
    Updated,
    Enabled,
    Disabled,
    Deleted,
    Revived,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerMutationReceipt {
    pub owner_scope: TriggerOwnerScope,
    pub subscription_key: String,
    pub subscription_id: String,
    pub incarnation: String,
    pub revision: u64,
    pub definition_hash: String,
    pub enabled: bool,
    pub disposition: TriggerMutationDisposition,
    pub record_snapshot: TriggerSubscriptionRecord,
}

impl TriggerMutationReceipt {
    fn from_record(
        record: TriggerSubscriptionRecord,
        disposition: TriggerMutationDisposition,
    ) -> Self {
        Self {
            owner_scope: record.owner_scope.clone(),
            subscription_key: record.subscription_key.clone(),
            subscription_id: record.subscription_id.clone(),
            incarnation: record.incarnation.clone(),
            revision: record.revision,
            definition_hash: record.definition_hash.clone(),
            enabled: record.enabled,
            disposition,
            record_snapshot: record,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TriggerCommand {
    Register {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        draft: TriggerSubscriptionDraft,
    },
    List {
        owner_scope: TriggerOwnerScope,
        filter: TriggerSubscriptionFilter,
    },
    Update {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        draft: TriggerSubscriptionDraft,
        expected_revision: u64,
    },
    Enable {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        expected_revision: u64,
    },
    Disable {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        expected_revision: u64,
    },
    Delete {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        expected_revision: u64,
    },
    Revive {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_key: String,
        draft: TriggerSubscriptionDraft,
        expected_revision: u64,
    },
    Prune {
        owner_scope: TriggerOwnerScope,
        actor: crate::ProcessOriginator,
        subscription_keys: Vec<String>,
    },
}

impl TriggerCommand {
    /// Exposes owner scope to store and durable-substrate implementors while persisting trigger
    /// subscriptions and occurrences.
    pub fn owner_scope(&self) -> &TriggerOwnerScope {
        match self {
            Self::Register { owner_scope, .. }
            | Self::List { owner_scope, .. }
            | Self::Update { owner_scope, .. }
            | Self::Enable { owner_scope, .. }
            | Self::Disable { owner_scope, .. }
            | Self::Delete { owner_scope, .. }
            | Self::Revive { owner_scope, .. }
            | Self::Prune { owner_scope, .. } => owner_scope,
        }
    }

    /// Exposes the single targeted subscription key to trigger-store implementors for register and
    /// point mutations, returning `None` for list and multi-key prune commands.
    pub fn subscription_key(&self) -> Option<&str> {
        match self {
            Self::Register { draft, .. } => Some(&draft.subscription_key),
            Self::List { .. } => None,
            Self::Update {
                subscription_key, ..
            }
            | Self::Enable {
                subscription_key, ..
            }
            | Self::Disable {
                subscription_key, ..
            }
            | Self::Delete {
                subscription_key, ..
            }
            | Self::Revive {
                subscription_key, ..
            } => Some(subscription_key),
            Self::Prune { .. } => None,
        }
    }

    /// Lets trigger-store implementors distinguish the read-only list command from every command
    /// that may change durable trigger state.
    pub fn is_mutation(&self) -> bool {
        !matches!(self, Self::List { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerCommandOutcome {
    Mutation {
        receipt: Box<TriggerMutationReceipt>,
    },
    List {
        records: Vec<TriggerSubscriptionRecord>,
    },
    Prune {
        receipts: Vec<TriggerMutationReceipt>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerOperationError {
    #[error(
        "trigger subscription conflict for `{subscription_key}`: {reason}; existing revision {existing_revision:?}, existing definition {existing_definition_hash:?}, requested definition {requested_definition_hash:?}"
    )]
    Conflict {
        subscription_key: String,
        existing_revision: Option<u64>,
        existing_definition_hash: Option<String>,
        requested_definition_hash: Option<String>,
        reason: String,
    },
    #[error("trigger subscription request is invalid: {message}")]
    Invalid { message: String },
    #[error("trigger subscription operation failed: {message}")]
    Store { message: String },
}

impl From<PluginError> for TriggerOperationError {
    fn from(value: PluginError) -> Self {
        Self::Store {
            message: value.to_string(),
        }
    }
}

pub type TriggerEffectResult = Result<TriggerCommandOutcome, TriggerOperationError>;

// Measured 112 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<TriggerEffectResult>() <= 144);

pub fn evaluate_trigger_prune(
    records: impl IntoIterator<Item = TriggerSubscriptionRecord>,
    owner_scope: TriggerOwnerScope,
    actor: crate::ProcessOriginator,
    subscription_keys: Vec<String>,
    now: u64,
) -> TriggerEffectResult {
    let requested = subscription_keys
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut receipts = Vec::new();
    for record in records {
        if record.owner_scope != owner_scope
            || record.tombstoned
            || !requested.contains(&record.subscription_key)
        {
            continue;
        }
        let command = TriggerCommand::Delete {
            owner_scope: owner_scope.clone(),
            actor: actor.clone(),
            subscription_key: record.subscription_key.clone(),
            expected_revision: record.revision,
        };
        match evaluate_trigger_mutation(Some(record), command, now)?? {
            TriggerCommandOutcome::Mutation { receipt } => receipts.push(*receipt),
            TriggerCommandOutcome::List { .. } | TriggerCommandOutcome::Prune { .. } => {
                unreachable!("delete mutation always returns one receipt")
            }
        }
    }
    receipts.sort_by(|left, right| left.subscription_key.cmp(&right.subscription_key));
    Ok(TriggerCommandOutcome::Prune { receipts })
}

pub fn trigger_command_hash(command: &TriggerCommand) -> Result<String, PluginError> {
    crate::stable_hash::stable_json_sha256_hex(command)
        .map_err(|err| PluginError::Session(format!("failed to hash trigger command: {err}")))
}

pub fn trigger_operation_receipt_id(
    owner_scope: &TriggerOwnerScope,
    operation_id: &str,
) -> Result<String, PluginError> {
    let digest = crate::stable_hash::stable_json_sha256_hex(&(
        "lash.trigger-operation-receipt",
        1_u8,
        owner_scope,
        operation_id,
    ))
    .map_err(|err| PluginError::Session(format!("failed to hash trigger operation id: {err}")))?;
    Ok(format!("trigger-operation:v1:sha256:{digest}"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerIngressResult {
    pub occurrence: TriggerOccurrenceRecord,
    pub reservations: Vec<TriggerDeliveryReservation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerDeliveryReservationStatus {
    Reserved,
    AlreadyReserved,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerDeliveryReservation {
    pub occurrence: TriggerOccurrenceRecord,
    pub subscription: TriggerSubscriptionRecord,
    pub process_id: String,
    pub created_at_ms: u64,
    pub reservation_status: TriggerDeliveryReservationStatus,
}

/// Stable identity of one trigger-delivery row considered for retention.
///
/// The process id is carried alongside the delivery table's composite primary
/// key so a retention decision can delete only the exact row it observed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TriggerDeliveryRetentionCandidate {
    pub occurrence_id: String,
    pub subscription_id: String,
    pub process_id: String,
}

/// Orders delivery starts by the stable subscription identity fields captured
/// in each reservation snapshot.
///
/// Trigger emission is journal-order-sensitive: first ingress and every
/// idempotent replay must visit the same subscriptions in the same order on
/// every backend. A tie on `(owner_scope, subscription_key)` is impossible in
/// valid state because that exact projection is unique and `subscription_id`
/// is derived from it. The final tie-breaker only defends corrupted snapshots;
/// if reached, it remains replay-stable because the ID is content-derived.
pub fn sort_trigger_delivery_reservations(reservations: &mut [TriggerDeliveryReservation]) {
    reservations.sort_by(|left, right| {
        left.subscription
            .owner_scope
            .namespace()
            .cmp(&right.subscription.owner_scope.namespace())
            .then_with(|| {
                left.subscription
                    .subscription_key
                    .cmp(&right.subscription.subscription_key)
            })
            .then_with(|| {
                left.subscription
                    .subscription_id
                    .cmp(&right.subscription.subscription_id)
            })
    });
}

impl TriggerDeliveryReservation {
    fn emit_report(&self, outcome: TriggerDeliveryEmitOutcome) -> TriggerDeliveryEmitReport {
        TriggerDeliveryEmitReport {
            occurrence_id: self.occurrence.occurrence_id.clone(),
            subscription_id: self.subscription.subscription_id.clone(),
            process_id: self.process_id.clone(),
            outcome,
        }
    }
}

#[async_trait::async_trait]
pub trait TriggerStore: Send + Sync {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: TriggerCommand,
    ) -> Result<TriggerEffectResult, PluginError>;

    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> Result<Vec<TriggerSubscriptionRecord>, PluginError>;

    async fn delete_session_subscriptions(&self, session_id: &str) -> Result<usize, PluginError>;

    async fn ingest_occurrence(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> Result<TriggerIngressResult, PluginError>;

    async fn list_occurrences(
        &self,
        filter: TriggerOccurrenceFilter,
    ) -> Result<Vec<TriggerOccurrenceRecord>, PluginError>;

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError>;

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError>;

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError>;

    /// List every reserved delivery snapshot, including deliveries whose live
    /// subscription has since been updated or tombstoned. Recovery uses this
    /// direct delivery-table view to close the reserve/start crash window.
    async fn list_deliveries(&self) -> Result<Vec<TriggerDeliveryReservation>, PluginError>;

    /// List the distinct deterministic process ids currently referenced by
    /// delivery rows, without materializing occurrence or subscription JSON.
    /// Process-retention reconciliation uses this narrow worklist query.
    async fn list_delivery_process_ids(&self) -> Result<Vec<String>, PluginError>;

    /// List the stable row identities used by process-retention reconciliation.
    ///
    /// The returned identity is an observation token: later deletion must match
    /// all three fields and must not expand to other rows that happen to carry
    /// the same reusable process id.
    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<TriggerDeliveryRetentionCandidate>, PluginError>;

    /// Delete only the supplied, previously observed delivery rows.
    ///
    /// Every candidate must match both the delivery row's composite identity
    /// and its observed process id. Repeating the deletion is harmless; a row
    /// inserted or replaced after classification is never swept into the batch.
    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, PluginError>;

    /// Low-level primitive for dropping mutation idempotency receipts older
    /// than an explicit cutoff. Process retention never prunes these receipts,
    /// and list operations never create them. Lash deliberately exposes no
    /// public facade or production schedule until FIG-653 can prove terminal-
    /// gated eligibility for each receipt.
    async fn prune_mutation_receipts(&self, cutoff_epoch_ms: u64) -> Result<usize, PluginError>;
}
