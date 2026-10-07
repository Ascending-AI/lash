use crate::ProcessId;
use crate::SessionId;
use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::plugin::PluginError;

mod command;
mod mutation;
mod report;
mod revision_referrer;
mod router;
mod start;
mod store_support;
mod subscription_changes;
#[cfg(test)]
mod tests;
mod trace_scope;

use crate::runtime::process::identity_projection::project_process_payload_leaf;
pub use command::*;
pub use mutation::{
    evaluate_trigger_mutation, evaluate_trigger_mutation_with_incarnation, trigger_incarnation,
};
pub use report::{TriggerDeliveryEmitOutcome, TriggerDeliveryEmitReceipt, TriggerEmitReport};
pub use revision_referrer::RevisionReferrerTriggerStore;
use router::default_enabled;
pub use router::*;
use router::{project_trigger_actor, project_trigger_draft, project_trigger_owner};
pub use start::{TriggerDeliveryStartRows, TriggerStartRows, TriggerSubscriptionFence};
pub use store_support::{
    PreparedTriggerCommand, TriggerMutationPreparation, decode_trigger_delivery,
    decode_trigger_delivery_outcome, decode_trigger_mutation_receipt_json,
    decode_trigger_occurrence_json, decode_trigger_subscription_json, encode_trigger_row,
    prepare_trigger_command, stored_trigger_receipt, trigger_mutation_records,
};
pub use subscription_changes::{TriggerSubscriptionChange, TriggerSubscriptionChangeCursor};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerEvent {
    pub resource_type: String,
    pub alias: String,
    pub event: String,
    pub payload_schema: crate::JsonSchema,
}

impl TriggerEvent {
    pub fn new(
        resource_type: impl Into<String>,
        alias: impl Into<String>,
        event: impl Into<String>,
        payload_schema: crate::JsonSchema,
    ) -> Self {
        Self {
            resource_type: resource_type.into(),
            alias: alias.into(),
            event: event.into(),
            payload_schema,
        }
    }

    pub fn payload_schema(&self) -> &crate::JsonSchema {
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
}

/// Terminal fate of one observed trigger occurrence.
///
/// Every record carries its outcome explicitly. Non-fired outcomes are
/// durable audit records: they never reserve trigger deliveries and are exempt
/// from delivery-fan-out reclamation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerOccurrenceOutcome {
    /// The occurrence fired and may reserve matching deliveries.
    #[default]
    Fired,
    /// The occurrence was deliberately dropped at an observed decision point.
    Dropped {
        /// Stable host-defined reason code for the drop.
        reason: String,
    },
}

impl TriggerOccurrenceOutcome {
    /// The durable SQL discriminant used for delivery and retention eligibility.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Fired => "fired",
            Self::Dropped { .. } => "dropped",
        }
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
    pub session_id: Option<SessionId>,
    pub outcome: TriggerOccurrenceOutcome,
    /// What the fire offers the occurrence's trace scope: its cause and the
    /// anchor its admission candidate proposed. The ingest that inserts the
    /// occurrence retains them as the record's
    /// [`trace`](TriggerOccurrenceRecord::trace); a redelivery under the
    /// same idempotency key reads the retained scope back and drops the
    /// offer. It is no part of the occurrence's identity or of what a
    /// redelivery is matched on.
    #[serde(default, skip_serializing_if = "lash_trace::TraceScopeOffer::is_empty")]
    pub trace: lash_trace::TraceScopeOffer,
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
            outcome: TriggerOccurrenceOutcome::Fired,
            trace: lash_trace::TraceScopeOffer::default(),
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
    pub fn for_session(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_outcome(mut self, outcome: TriggerOccurrenceOutcome) -> Self {
        self.outcome = outcome;
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
    pub session_id: Option<SessionId>,
    pub outcome: TriggerOccurrenceOutcome,
    pub occurred_at_ms: u64,
    /// The fire's trace scope: the cause and anchor its first ingest
    /// retained, started at `occurred_at_ms`. Every delivery the occurrence
    /// reserves carries this record, so each run it starts links the same
    /// fire. `None` on a record written without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<lash_trace::DurableTraceScope>,
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
    pub source_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source_type: TriggerEventType,
    pub source: serde_json::Value,
    pub target: TriggerTarget,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerTarget {
    pub label: Option<String>,
    pub identity: crate::ProcessIdentity,
    pub input: crate::ProcessStartTarget,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, TriggerInputBinding>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerInputBinding {
    Event,
    Fixed { value: serde_json::Value },
}

/// The authorized route a trigger subscription pins for its source.
///
/// Core records the route and never interprets, widens or re-derives one. A
/// resident source needs none: its definition travels inside the captured
/// execution requirements the subscription already pins. A source admitted from
/// a deferred trigger provider carries that provider's identity and its opaque
/// routing reference, exactly as the grant delivered it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerProviderRoute {
    /// The source is resident in the captured execution requirements.
    Resident,
    /// A trigger provider authorized this route at link time.
    Provider {
        provider_id: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        route: serde_json::Value,
    },
}

impl TriggerProviderRoute {
    /// The provider that authorized this route, or `None` for a resident source.
    pub fn provider_id(&self) -> Option<&str> {
        match self {
            Self::Resident => None,
            Self::Provider { provider_id, .. } => Some(provider_id),
        }
    }

    /// The opaque routing reference, or `None` for a resident source.
    pub fn route(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Resident => None,
            Self::Provider { route, .. } => Some(route),
        }
    }
}

/// The admitted source contract and provider route one subscription captured
/// when its registration executed.
///
/// A definition resolved in a foreground session cannot be the only record a
/// durable subscription relies on: a later catalog edit would silently change
/// which event shape is delivered and where it is routed. Registration copies
/// the admitted contract here, and every later delivery validates against this
/// capture rather than against whatever the live catalog now says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerSourceCapture {
    /// Fully qualified source-constructor path the registration admitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constructor_path: Vec<String>,
    /// The configuration contract that constructor declared.
    pub config_schema: crate::JsonSchema,
    /// The authorized provider route, opaque to core.
    pub route: TriggerProviderRoute,
}

impl TriggerSourceCapture {
    /// Captures a resident source: no provider route, only its contract.
    pub fn resident(
        constructor_path: impl IntoIterator<Item = impl Into<String>>,
        config_schema: crate::JsonSchema,
    ) -> Self {
        Self {
            constructor_path: constructor_path.into_iter().map(Into::into).collect(),
            config_schema,
            route: TriggerProviderRoute::Resident,
        }
    }

    /// Captures a provider-admitted source with its opaque authorized route.
    pub fn provider(
        constructor_path: impl IntoIterator<Item = impl Into<String>>,
        config_schema: crate::JsonSchema,
        provider_id: impl Into<String>,
        route: serde_json::Value,
    ) -> Self {
        Self {
            constructor_path: constructor_path.into_iter().map(Into::into).collect(),
            config_schema,
            route: TriggerProviderRoute::Provider {
                provider_id: provider_id.into(),
                route,
            },
        }
    }

    /// A capture that names no contract, for the host-owned subscriptions whose
    /// source is not a linked constructor.
    pub fn untyped() -> Self {
        Self::resident(Vec::<String>::new(), crate::JsonSchema::any())
    }

    /// The provider that authorized the route, or `None` for a resident source.
    pub fn provider_id(&self) -> Option<&str> {
        self.route.provider_id()
    }

    /// Rejects a provider route with no provider identity, which would name an
    /// authority no host can restore.
    pub fn validate(&self) -> Result<(), PluginError> {
        if let TriggerProviderRoute::Provider { provider_id, .. } = &self.route
            && provider_id.trim().is_empty()
        {
            return Err(PluginError::Session(
                "trigger source capture carries a provider route with no provider id".to_string(),
            ));
        }
        Ok(())
    }
}

/// Why a captured provider route could not be restored for one delivery.
///
/// The two cases are not interchangeable. A provider that is temporarily down
/// has said nothing about the authorization, so the reserved work stays durable
/// and the next attempt retries the same delivery identity. A revoked or
/// incompatible route is a decision: the attempt refuses visibly, the
/// reservation is neither deleted nor marked started, and nothing re-resolves
/// the source against a live catalog to find a replacement grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerRouteRefusal {
    /// The provider is unreachable right now. Retryable, same identity.
    Unavailable {
        provider_id: String,
        message: String,
    },
    /// The provider revoked or refuses this route. Terminal for the attempt.
    Revoked {
        provider_id: String,
        message: String,
    },
}

impl TriggerRouteRefusal {
    /// Whether the runtime may retry this delivery under the same identity.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

impl std::fmt::Display for TriggerRouteRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable {
                provider_id,
                message,
            } => write!(
                formatter,
                "trigger provider `{provider_id}` is temporarily unavailable: {message}"
            ),
            Self::Revoked {
                provider_id,
                message,
            } => write!(
                formatter,
                "trigger provider `{provider_id}` refuses the captured route: {message}"
            ),
        }
    }
}

/// The refusal as a delivery start's recorded outcome: its class stays typed
/// in the error's code (FIG-4554).
impl From<TriggerRouteRefusal> for crate::RuntimeEffectControllerError {
    fn from(refusal: TriggerRouteRefusal) -> Self {
        let code = match &refusal {
            TriggerRouteRefusal::Unavailable { .. } => {
                crate::RuntimeErrorCode::TriggerRouteUnavailable
            }
            TriggerRouteRefusal::Revoked { .. } => crate::RuntimeErrorCode::TriggerRouteRevoked,
        };
        Self::new(code, refusal.to_string())
    }
}

/// Reinstalls a captured provider route before a delivery executes.
///
/// It is a live host service that serves new work only (FIG-4554). A
/// delivery's start asks it inside the start's recorded admission, and only
/// while no process holds the start's key, so its answer, a refusal
/// included, is that step's recorded outcome. A replay of the emission reads
/// the record, and a redrive finds the process the key holds; neither asks
/// again. It may not widen the grant, consult a catalog, or resolve a
/// replacement definition: the capture is the whole authority, and the only
/// answers are "restored", "not right now", and "refused".
#[async_trait::async_trait]
pub trait TriggerRouteRestorer: Send + Sync {
    async fn restore(&self, capture: &TriggerSourceCapture) -> Result<(), TriggerRouteRefusal>;
}

/// One delivery start's route restore: the host's restorer, and the capture
/// the delivery's reservation recorded (FIG-4554).
///
/// The router hands it to the start's executor, and the start's recorded
/// admission ([`register_process_start`](crate::runtime::register_process_start))
/// consults it. It never travels in the start's command: the restorer is the
/// host's wiring today, not part of what the journal records.
#[derive(Clone)]
pub struct TriggerRouteRestore(Arc<RouteRestore>);

struct RouteRestore {
    restorer: Arc<dyn TriggerRouteRestorer>,
    capture: TriggerSourceCapture,
}

impl TriggerRouteRestore {
    pub fn new(restorer: Arc<dyn TriggerRouteRestorer>, capture: TriggerSourceCapture) -> Self {
        Self(Arc::new(RouteRestore { restorer, capture }))
    }

    pub(crate) fn restorer(&self) -> &dyn TriggerRouteRestorer {
        self.0.restorer.as_ref()
    }

    pub(crate) fn capture(&self) -> &TriggerSourceCapture {
        &self.0.capture
    }
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
    pub payload_schema: crate::JsonSchema,
    /// The admitted source contract and provider route this registration
    /// captured. Required: a subscription with no capture cannot validate a
    /// delivery or restore its route, so a record written before the capture
    /// existed is refused rather than defaulted into a false authority.
    pub source_capture: TriggerSourceCapture,
    pub target: crate::ProcessStartTarget,
    pub target_identity: crate::ProcessIdentity,
    #[serde(default)]
    pub event_types: Vec<crate::ProcessEventType>,
    #[serde(default)]
    pub input_template: BTreeMap<String, TriggerInputBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
}

impl TriggerSubscriptionDraft {
    #[expect(
        clippy::expect_used,
        reason = "this module declares the tool or payload schema and admission checks its invariant"
    )]
    pub fn for_process(
        subscription_key: impl Into<String>,
        env_ref: crate::ProcessExecutionEnvRef,
        source_type: impl Into<String>,
        source_key: impl Into<String>,
        target: impl Into<crate::ProcessStartTarget>,
        target_identity: crate::ProcessIdentity,
    ) -> Self {
        let target = target.into();
        let target_label = target_identity.label.clone();
        Self {
            subscription_key: subscription_key.into(),
            env_ref,
            wake_target: None,
            name: None,
            source_type: source_type.into(),
            source_key: source_key.into(),
            source: serde_json::Value::Object(serde_json::Map::new()),
            payload_schema: crate::JsonSchema::admit(serde_json::Value::Object(
                serde_json::Map::new(),
            ))
            .expect("valid declared payload schema"),
            source_capture: TriggerSourceCapture::untyped(),
            target,
            target_identity,
            event_types: Vec::new(),
            input_template: BTreeMap::new(),
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

    pub fn with_payload_schema(mut self, payload_schema: crate::JsonSchema) -> Self {
        self.payload_schema = payload_schema;
        self
    }

    /// Sets the admitted source contract and provider route captured by a
    /// `TriggerSubscriptionDraft`, which every later delivery validates and
    /// routes against instead of consulting the live catalog.
    pub fn with_source_capture(mut self, source_capture: TriggerSourceCapture) -> Self {
        self.source_capture = source_capture;
        self
    }

    pub fn with_wake_target(mut self, wake_target: crate::SessionScope) -> Self {
        self.wake_target = Some(wake_target);
        self
    }

    pub fn with_event_types(
        mut self,
        event_types: impl IntoIterator<Item = crate::ProcessEventType>,
    ) -> Self {
        self.event_types = event_types.into_iter().collect();
        self
    }

    pub fn with_input_template(
        mut self,
        input_template: BTreeMap<String, TriggerInputBinding>,
    ) -> Self {
        self.input_template = input_template;
        self
    }

    pub fn with_target_label(mut self, target_label: impl Into<String>) -> Self {
        self.target_label = Some(target_label.into());
        self
    }

    /// Validates the subscription key, Engine target shape and captured source
    /// contract before trigger-store implementors persist the draft.
    pub fn validate(&self) -> Result<(), PluginError> {
        validate_subscription_key(&self.subscription_key, false)?;
        validate_trigger_target(&self.target)?;
        self.source_capture.validate()?;
        Ok(())
    }
}

fn validate_trigger_target(target: &crate::ProcessStartTarget) -> Result<(), PluginError> {
    match target {
        crate::ProcessStartTarget::Input(crate::ProcessInput::Engine { .. })
        | crate::ProcessStartTarget::Definition { .. } => Ok(()),
        crate::ProcessStartTarget::Input(input @ crate::ProcessInput::SessionTurn { .. }) => {
            Err(PluginError::InvalidTriggerTarget {
                kind: input.engine_kind().to_string(),
            })
        }
    }
}

pub const INTERNAL_TRIGGER_KEY_PREFIX: &str = "lash.internal/";

pub fn validate_subscription_key(key: &str, internal: bool) -> Result<(), PluginError> {
    if !crate::store::namespace::is_valid_opaque_key(key.trim()) {
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
    Session { session_id: SessionId },
    Host { binding_id: String },
    Platform,
}

impl TriggerOwnerScope {
    pub fn session(session_id: impl Into<SessionId>) -> Self {
        Self::Session {
            session_id: session_id.into(),
        }
    }

    /// Constructs host-owned trigger scope for store implementors and rejects an empty binding ID
    /// so its durable namespace cannot collapse.
    pub fn host(binding_id: impl Into<String>) -> Result<Self, PluginError> {
        let binding_id = binding_id.into();
        if !crate::store::namespace::is_valid_opaque_key(binding_id.trim()) {
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
            Self::Session { session_id } => lash_sansio::session_owner_namespace(session_id),
            Self::Host { binding_id } => format!("host:{binding_id}"),
            Self::Platform => "host".to_string(),
        }
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Session { session_id } => Some(session_id),
            Self::Host { .. } | Self::Platform => None,
        }
    }

    /// The durable owner-kind vocabulary both backends store in `owner_kind`
    /// and constrain by `CHECK` (FIG-1956).
    pub fn owner_kind_column(&self) -> &'static str {
        match self {
            Self::Session { .. } => "session",
            Self::Host { .. } => "host",
            Self::Platform => "platform",
        }
    }

    /// The durable owner id stored beside [`Self::owner_kind_column`]: the
    /// session id, the host binding id, or the fixed `platform` sentinel.
    pub fn owner_id_column(&self) -> &str {
        match self {
            Self::Session { session_id } => session_id.as_str(),
            Self::Host { binding_id } => binding_id,
            Self::Platform => "platform",
        }
    }
}

/// The durable lifecycle of one trigger subscription.
///
/// Three states, one carrier. The tombstone's deletion time lives in the only
/// variant where it means anything, so a tombstone without a time — and a time
/// without a tombstone — is unrepresentable in the type, in both backends'
/// column pair, and in the wire tag simultaneously.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "lifecycle",
    content = "deleted_at_ms",
    rename_all = "snake_case"
)]
pub enum TriggerSubscriptionLifecycle {
    /// Live and routable: the router delivers matching occurrences.
    Enabled,
    /// Live but not routable: the subscription is retained and revisionable,
    /// and an `Enable` returns it to service.
    Disabled,
    /// Fenced: the key stays unique on the owner scope, no consumer resolves
    /// it, and only `Revive` brings it back under a new incarnation. The
    /// payload is when the tombstone was taken, and it is carried flat as the
    /// `deleted_at_ms` content so the JSON is isomorphic to the two backend
    /// columns the paired-nullable CHECK policies.
    Tombstoned(u64),
}

impl TriggerSubscriptionLifecycle {
    pub fn routable(&self) -> bool {
        matches!(self, Self::Enabled)
    }

    /// Whether the key is fenced behind a tombstone.
    pub fn is_tombstoned(&self) -> bool {
        matches!(self, Self::Tombstoned { .. })
    }

    /// The host-facing enabled flag: `Enabled` alone, never a tombstone.
    pub fn enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }

    /// When the tombstone was taken, for the tombstoned state only.
    pub fn deleted_at_ms(&self) -> Option<u64> {
        match self {
            Self::Tombstoned(deleted_at_ms) => Some(*deleted_at_ms),
            Self::Enabled | Self::Disabled => None,
        }
    }

    /// The durable column vocabulary both backends store and `CHECK`.
    pub fn as_column(&self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Tombstoned { .. } => "tombstoned",
        }
    }

    /// Rebuilds a lifecycle from the backend column pair, refusing every
    /// combination the `CHECK` constraints forbid.
    pub fn from_columns(
        lifecycle: &str,
        deleted_at_ms: Option<u64>,
    ) -> Result<Self, TriggerLifecycleColumnError> {
        match (lifecycle, deleted_at_ms) {
            ("enabled", None) => Ok(Self::Enabled),
            ("disabled", None) => Ok(Self::Disabled),
            ("tombstoned", Some(deleted_at_ms)) => Ok(Self::Tombstoned(deleted_at_ms)),
            ("enabled" | "disabled" | "tombstoned", _) => {
                Err(TriggerLifecycleColumnError::MispairedDeletedAt {
                    lifecycle: lifecycle.to_string(),
                    deleted_at_ms,
                })
            }
            _ => Err(TriggerLifecycleColumnError::UnknownLifecycle {
                lifecycle: lifecycle.to_string(),
            }),
        }
    }
}

/// Why a stored trigger-subscription lifecycle column pair is not a lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TriggerLifecycleColumnError {
    /// The column holds a word outside the stored vocabulary.
    #[error("unknown trigger subscription lifecycle {lifecycle:?}")]
    UnknownLifecycle {
        /// The refused column value.
        lifecycle: String,
    },
    /// The deletion timestamp disagrees with the lifecycle it is paired with.
    #[error(
        "trigger subscription lifecycle {lifecycle:?} cannot carry deleted_at_ms {deleted_at_ms:?}"
    )]
    MispairedDeletedAt {
        /// The lifecycle column value.
        lifecycle: String,
        /// The deletion timestamp column value.
        deleted_at_ms: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerSubscriptionRecord {
    pub subscription_id: String,
    pub owner_scope: TriggerOwnerScope,
    pub subscription_key: String,
    pub incarnation: String,
    pub revision: u64,
    pub definition_fingerprint: String,
    pub registrant: crate::ProcessOriginator,
    pub env_ref: crate::ProcessExecutionEnvRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_target: Option<crate::SessionScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub source_type: String,
    pub source_key: String,
    pub source: serde_json::Value,
    pub payload_schema: crate::JsonSchema,
    /// The source contract and provider route admitted at registration. A row
    /// written before the capture existed has no authority to deliver against
    /// and is refused at decode; see the store's format-version refusal.
    pub source_capture: TriggerSourceCapture,
    pub target: crate::ProcessStartTarget,
    pub target_identity: crate::ProcessIdentity,
    #[serde(default)]
    pub event_types: Vec<crate::ProcessEventType>,
    #[serde(default)]
    pub input_template: BTreeMap<String, TriggerInputBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
    /// The one lifecycle fact this row carries (FIG-1951). Replaces the
    /// `enabled`/`tombstoned`/`deleted_at_ms` triple, whose eight
    /// representable combinations spelled three legal states.
    pub lifecycle: TriggerSubscriptionLifecycle,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl TriggerSubscriptionRecord {
    /// The single liveness predicate: before FIG-1951 the same
    /// `enabled && !tombstoned` conjunction was spelled once in the router and
    /// once in each store's SQL.
    pub fn routable(&self) -> bool {
        self.lifecycle.routable()
    }

    /// Whether the subscription key is fenced and needs a revive to return.
    pub fn is_tombstoned(&self) -> bool {
        self.lifecycle.is_tombstoned()
    }

    /// Takes the tombstone at `now`.
    ///
    /// The single tombstone transition: before FIG-1951 four call sites wrote
    /// the three fields by hand, and one of them set three fields on the
    /// record while its SQL `UPDATE` set two.
    pub fn tombstone(&mut self, now: u64) {
        self.lifecycle = TriggerSubscriptionLifecycle::Tombstoned(now);
    }

    /// Projects the canonical owner namespace for trigger-store implementors filtering records
    /// across session, host, and platform registrants.
    pub fn registrant_scope_id(&self) -> String {
        self.owner_scope.namespace()
    }

    pub fn registrant_session_id(&self) -> Option<&SessionId> {
        self.owner_scope.session_id()
    }
}

impl From<&TriggerSubscriptionRecord> for TriggerRegistration {
    fn from(route: &TriggerSubscriptionRecord) -> Self {
        Self {
            subscription_key: route.subscription_key.clone(),
            incarnation: route.incarnation.clone(),
            revision: route.revision,
            registrant: route.registrant.clone(),
            source_key: route.source_key.clone(),
            name: route.name.clone(),
            source_type: TriggerEventType::new(route.source_type.clone()),
            source: route.source.clone(),
            target: TriggerTarget {
                label: route.target_label.clone(),
                identity: route.target_identity.clone(),
                input: route.target.clone(),
                inputs: route.input_template.clone(),
            },
            enabled: route.lifecycle.enabled(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerSubscriptionFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registrant_scope_id: Option<String>,
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
    pub fn for_session(session_id: impl Into<SessionId>) -> Self {
        Self::for_registrant_scope(lash_sansio::session_owner_namespace(session_id.into()))
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

    /// Applies every populated subscription filter conjunctively for trigger-store and conformance
    /// implementors and always excludes tombstoned records.
    pub fn matches(&self, record: &TriggerSubscriptionRecord) -> bool {
        self.registrant_scope_id
            .as_deref()
            .is_none_or(|scope_id| record.registrant_scope_id() == scope_id)
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
            && self
                .enabled
                .is_none_or(|enabled| record.lifecycle.enabled() == enabled)
            && !record.is_tombstoned()
            && self.target.as_ref().is_none_or(|target| {
                record
                    .target_identity
                    .definition_id
                    .as_ref()
                    .is_some_and(|id| &id.to_tagged_json() == target)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerIngressReceipt {
    pub occurrence: TriggerOccurrenceRecord,
    pub reservations: Vec<TriggerDeliveryReservation>,
    /// Whether this call recorded the occurrence, or the store already held one
    /// under the same idempotency key and returned it (FIG-3070).
    #[serde(default, skip_serializing_if = "crate::StoreRealization::is_realized")]
    pub realization: crate::StoreRealization,
}

/// What recording an occurrence would do, read before its start: the
/// answer [`TriggerStore::plan_occurrence`] gives.
#[derive(Clone, Debug, PartialEq)]
pub enum TriggerOccurrencePlan {
    /// The occurrence is already recorded under its idempotency key: what it
    /// holds, every delivery bound to its process.
    Held(TriggerIngressReceipt),
    /// The occurrence is new: its record, stamped on the store's clock, and
    /// the enabled subscriptions it matches, which its start prepares a
    /// process for each of and records only while they still match.
    Fresh {
        occurrence: TriggerOccurrenceRecord,
        subscriptions: Vec<TriggerSubscriptionRecord>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerDeliveryReservation {
    pub occurrence: TriggerOccurrenceRecord,
    pub subscription: TriggerSubscriptionRecord,
    /// The disposition recorded with the occurrence: its started process or
    /// its terminal refusal (ADR 0132 §12).
    pub outcome: TriggerDeliveryEmitOutcome,
    pub created_at_ms: u64,
}

impl TriggerDeliveryReservation {
    /// The process this delivery started, absent for a refused delivery.
    #[must_use]
    pub fn process_id(&self) -> Option<&ProcessId> {
        match &self.outcome {
            TriggerDeliveryEmitOutcome::Started { process_id } => Some(process_id),
            TriggerDeliveryEmitOutcome::Failed { .. } => None,
        }
    }
}

/// Stable identity used by store implementors during delivery retention.
///
/// The process id is carried alongside the delivery table's composite primary
/// key so a retention decision can delete only the exact row it observed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TriggerDeliveryRetentionCandidate {
    pub occurrence_id: String,
    pub subscription_id: String,
    pub process_id: ProcessId,
}

/// Counters produced by store implementors after atomic reconciliation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerRetentionReconciliationReport {
    /// Exact delivery observations removed by the reconciliation.
    pub reclaimed_delivery_count: usize,
    /// Occurrences removed after their complete delivery fan-out became empty.
    pub reclaimed_occurrence_count: usize,
    /// Deleted-session subscriptions removed after their deliveries were gone.
    pub reclaimed_subscription_count: usize,
}

/// Outcome counters from one host-invoked trigger-occurrence reclaim pass.
///
/// Occurrences enter the reclaimable set only at ingest accounting for a
/// zero-match fan-out or when deletion of the final delivery severs a matched
/// fan-out. `cutoff_epoch_ms` on
/// [`TriggerStore::reclaim_trigger_occurrences`] can defer those armed rows;
/// it never makes a live fan-out reclaimable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerOccurrenceReclamationReport {
    /// Occurrences observed across the completely enumerated scope.
    pub inspected_occurrence_count: usize,
    pub reclaimed_occurrence_count: usize,
    /// Occurrences whose delivery fan-out is still live and therefore has not
    /// armed reclaim eligibility. Non-fired audit rows are never counted here:
    /// they have no fan-out to be stuck on.
    pub live_fan_out_count: usize,
    /// Non-fired occurrences observed and left alone. They are durable audit
    /// history (ADR 0067), so they are not a blocker and never make the sweep
    /// incomplete; only [`TriggerStore::prune_non_fired_occurrences`] reclaims
    /// them.
    pub audit_retained_count: usize,
    pub grace_deferred_count: usize,
    /// Eligible occurrences whose per-row delete no longer matched after the
    /// scope snapshot. A concurrent maintenance pass may already have removed
    /// them; a fresh pass must re-inspect the scope before reporting witnessed
    /// emptiness.
    pub reinspection_deferred_count: usize,
}

impl crate::store::MaintenanceReport for TriggerOccurrenceReclamationReport {
    fn reclaimed_count(&self) -> usize {
        self.reclaimed_occurrence_count
    }

    fn sweep(&self) -> crate::store::MaintenanceSweep {
        if self.live_fan_out_count > 0
            || self.grace_deferred_count > 0
            || self.reinspection_deferred_count > 0
        {
            crate::store::MaintenanceSweep::Incomplete
        } else if self.reclaimed_occurrence_count > 0 {
            crate::store::MaintenanceSweep::Swept
        } else {
            crate::store::MaintenanceSweep::NothingToDo
        }
    }
}

/// A trigger-occurrence reclaim pass either completes with its counters or
/// fails honestly with the counters accumulated before the failing delete.
pub type TriggerOccurrenceReclamationResult = Result<
    TriggerOccurrenceReclamationReport,
    crate::store::MaintenanceFailure<TriggerOccurrenceReclamationReport, Box<PluginError>>,
>;

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
    reservations.sort_by(|left, right| subscription_order(&left.subscription, &right.subscription));
}

/// [`sort_trigger_delivery_reservations`]'s order over the subscriptions an
/// occurrence's plan matched, so its deliveries start in the order they are
/// later reported.
pub fn sort_trigger_subscriptions(subscriptions: &mut [TriggerSubscriptionRecord]) {
    subscriptions.sort_by(subscription_order);
}

fn subscription_order(
    left: &TriggerSubscriptionRecord,
    right: &TriggerSubscriptionRecord,
) -> std::cmp::Ordering {
    left.owner_scope
        .namespace()
        .cmp(&right.owner_scope.namespace())
        .then_with(|| left.subscription_key.cmp(&right.subscription_key))
        .then_with(|| left.subscription_id.cmp(&right.subscription_id))
}

/// Store and durable-substrate implementors provide this durable home for
/// trigger subscriptions, occurrences, and delivery reservations, separate
/// from [`RuntimeStore`](crate::RuntimeStore). Store tables and
/// record JSON are private to Lash; hosts use [`TriggerCommand`] and the Lash
/// facade instead of reading or writing backend tables directly.
#[async_trait::async_trait]
pub trait TriggerStore: Send + Sync {
    /// Evaluate one [`TriggerCommand`] and, for a mutation, commit its record
    /// change and its receipt in a single transaction.
    ///
    /// The outer `Result` reports store failure; [`TriggerEffectResult`] carries
    /// the domain outcome, including a losing revision fence. `operation_id` is
    /// an idempotency key within the [`TriggerOwnerScope`]: a mutation journals
    /// its result, so reuse it only to retry the same intent. Lists are not
    /// receipted and always read current state.
    async fn execute_command(
        &self,
        operation_id: &str,
        command: TriggerCommand,
    ) -> Result<TriggerEffectResult, PluginError>;

    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> Result<Vec<TriggerSubscriptionRecord>, PluginError>;

    /// Read each subscription's latest state in change-sequence order.
    /// Multiple edits coalesce. A zero limit leaves the cursor unchanged.
    /// A cursor below the retained tombstone horizon returns
    /// `PluginError::TriggerSubscriptionChangeCursorPruned`.
    async fn subscriptions_changed_since(
        &self,
        cursor: TriggerSubscriptionChangeCursor,
        limit: usize,
    ) -> Result<
        (
            Vec<TriggerSubscriptionChange>,
            TriggerSubscriptionChangeCursor,
        ),
        PluginError,
    >;

    /// Atomically list all live subscriptions and the cursor at that snapshot.
    /// Use this after an expired cursor, reconciling absent ids as deletions.
    async fn list_subscriptions_with_cursor(
        &self,
    ) -> Result<
        (
            Vec<TriggerSubscriptionRecord>,
            TriggerSubscriptionChangeCursor,
        ),
        PluginError,
    >;

    /// Forget deletion evidence strictly older than this host-chosen time.
    /// Lagging consumers must resync. Returns the number of removed tombstones.
    async fn compact_subscription_tombstones(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, PluginError>;

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, PluginError>;

    /// Read what recording `request`'s occurrence would do: the occurrence
    /// and deliveries already held under its idempotency key, or a new
    /// record stamped now with the enabled subscriptions it matches. Writes
    /// nothing; the occurrence is recorded by its start's `trigger.start`
    /// commit, which records it only while the plan still holds.
    ///
    /// An identity retention has reclaimed is never written back while its
    /// tombstone remains (FIG-4513, FIG-4610): a plan that finds the
    /// tombstone refuses with
    /// [`trigger_occurrence_reclaimed`](crate::trigger_occurrence_reclaimed),
    /// a redelivery of an emission that already ran. The tombstone remains
    /// until the host explicitly deletes it with
    /// [`Self::forget_trigger_tombstones`].
    async fn plan_occurrence(
        &self,
        request: &TriggerOccurrenceRequest,
    ) -> Result<TriggerOccurrencePlan, PluginError>;

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
        process_id: &ProcessId,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError>;

    /// List every reserved delivery snapshot, including deliveries whose live
    /// subscription has since been updated or tombstoned: the direct
    /// delivery-table view.
    async fn list_deliveries(&self) -> Result<Vec<TriggerDeliveryReservation>, PluginError>;

    /// List the distinct process ids currently bound to delivery rows,
    /// without materializing occurrence or subscription JSON.
    /// Process-retention reconciliation uses this narrow worklist query.
    async fn list_delivery_process_ids(&self) -> Result<Vec<ProcessId>, PluginError>;

    /// List stable observations for process-retention reconciliation. Deletion
    /// must match every identity field, never just the reusable process id.
    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<TriggerDeliveryRetentionCandidate>, PluginError>;

    /// Store implementors list session owners found in subscriptions, delivery
    /// snapshots, or receipts for ADR 0049 frontier classification.
    async fn list_session_owner_ids_for_retention(&self) -> Result<Vec<SessionId>, PluginError>;

    /// Store implementors apply one trigger-retention decision atomically.
    ///
    /// Host and platform subscription fences are never selected.
    async fn reconcile_trigger_retention(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<TriggerRetentionReconciliationReport, PluginError>;

    /// Delete only the supplied, previously observed delivery rows.
    ///
    /// Candidates match the composite row identity and observed process id, so
    /// repetition is harmless and later replacement rows are not swept in.
    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, PluginError>;

    /// Reclaim terminal trigger occurrences armed no later than a host cutoff.
    /// This host lever deletes terminally armed occurrences: zero-match rows arm
    /// at ingest, while matched rows arm with their last delivery's deletion.
    /// The cutoff only defers eligibility. The complete scope is witnessed
    /// before deletion; later failure returns the partial report accumulated.
    ///
    /// Every occurrence delete writes its tombstone on the store's clock.
    /// The pass never deletes a tombstone, whatever the cutoff, including
    /// `u64::MAX`. Only [`Self::forget_trigger_tombstones`] deletes them.
    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> TriggerOccurrenceReclamationResult;

    /// Delete exactly the occurrence tombstones written strictly before
    /// `written_before_epoch_ms` on this store's clock, returning the count.
    ///
    /// The host vouches that its trigger source will no longer redeliver the
    /// selected occurrences. A later redelivery of a forgotten identity runs
    /// as a new occurrence. Tombstones are never deleted automatically.
    ///
    /// The deletion is one fenced transaction. A failure rolls it back and
    /// returns the typed store error, including writer-fence and contention
    /// causes.
    async fn forget_trigger_tombstones(
        &self,
        written_before_epoch_ms: u64,
    ) -> Result<usize, crate::StoreError>;

    /// Low-level primitive for dropping non-fired occurrence rows recorded
    /// before an explicit cutoff, returning the number deleted.
    ///
    /// Non-fired occurrences are durable audit history under ADR 0067: no
    /// delivery-fan-out retention path reclaims them, and
    /// [`TriggerStore::reclaim_trigger_occurrences`] still cannot reach them.
    /// This is the host's only escape hatch for a producer that records a
    /// non-fired outcome per tick, and it is deliberately explicit: the host
    /// names the audit window it is discarding. Fired occurrences are never
    /// selected, whatever the cutoff.
    async fn prune_non_fired_occurrences(&self, cutoff_epoch_ms: u64)
    -> Result<usize, PluginError>;
}

mod target_admission;
pub use target_admission::admit_trigger_registration_target;
