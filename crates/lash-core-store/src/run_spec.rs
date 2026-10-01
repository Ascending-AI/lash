//! The shape an accepted input runs under (FIG-3838, D5).
//!
//! Every accepted input carries a [`RunSpec`]: a registered run definition,
//! the context data that definition reads, and one-shot overrides of the
//! session config. The empty default spec is "the session config": it adds
//! nothing to an input's row or its submission digest, so an omitted spec
//! and an explicit default are the same submission.
//!
//! A non-default spec is interned per session under its [`RunSpecHash`]: the
//! input row names the hash and the session's spec table holds the canonical
//! bytes once. The hash is part of the input's submission digest, so a
//! same-id retry with a different spec is a conflict.
//!
//! The drive resolves a root's spec exactly once, against the root's config
//! snapshot taken after the boundary's command drain, and records the result
//! as the root's [`ResolvedRun`]. Every replay and worker hop reads the
//! record; the resolver never runs again for a recorded root. Overrides shape
//! that root only: they never reach the sticky session config.
//!
//! A claim never mixes specs: the next-turn prefix stops at the first input
//! whose spec differs from its head's. Steering joins the running root's
//! recorded shape, so an input addressed to a running turn under a differing
//! explicit spec is refused before acceptance.

use crate::session_graph::PersistedSessionConfig;
use crate::{GenerationOptions, ModelKey, ProtocolTurnOptions, ReasoningSelection};

/// Family version of the [`RunSpecHash`] preimage and of the canonical spec
/// bytes it hashes.
pub const RUN_SPEC_FAMILY_VERSION: u8 = 1;

/// An immutable run definition name and revision a deployment registers.
///
/// Resolution looks up the exact revision; a worker that does not serve it
/// retries and parks, and never falls back to another revision.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct DefinitionRef {
    pub name: String,
    pub revision: u32,
}

impl DefinitionRef {
    pub fn new(name: impl Into<String>, revision: u32) -> Self {
        Self {
            name: name.into(),
            revision,
        }
    }
}

impl std::fmt::Display for DefinitionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.revision)
    }
}

/// A slot a root's spec fills with a durable capability (D5): the name the
/// definition or the root's tooling binds the capability under.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SlotId(String);

impl SlotId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SlotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The binding a worker resolves a capability by: an exact id, never a live
/// object.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct BindingId(String);

impl BindingId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An immutable capability contract a deployment's adapters serve, by exact
/// name and revision.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ContractRef {
    pub name: String,
    pub revision: u32,
}

impl ContractRef {
    pub fn new(name: impl Into<String>, revision: u32) -> Self {
        Self {
            name: name.into(),
            revision,
        }
    }
}

/// A durable capability a spec names in one of its slots: the contract it is
/// bound under, the exact binding the worker resolves, and the binding's own
/// data (D5). Everything here is serializable — a live object cannot ride a
/// spec.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CapabilityRef {
    pub contract: ContractRef,
    pub binding: BindingId,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub args: serde_json::Value,
}

// `serde_json::Value` never holds NaN or an infinite number, so its
// `PartialEq` is reflexive and `CapabilityRef` can be `Eq` — which
// `ResolvedRun`'s `Eq` (carried on `PendingFollowOn`) requires.
impl Eq for CapabilityRef {}

/// One-shot overrides of the session config for the root that runs an
/// input. Each field left `None` keeps the root's snapshot value.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunOverrides {
    /// The model this root runs, by the host's key. The root resolves it once,
    /// when it records its shape: the registry mints the binding then, and
    /// every replay reads the recorded binding. The snapshot's reasoning
    /// stays unless [`Self::reasoning`] is set too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelKey>,
    /// The reasoning this root runs its model with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationOptions>,
    /// The options this run states for the session's protocol (RLM finish
    /// policy and schema included): the protocol owner's typed run options,
    /// which that owner alone applies to its recorded namespace. A snapshot
    /// that records no protocol plugin refuses them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_turn_options: Option<ProtocolTurnOptions>,
}

impl RunOverrides {
    #[expect(
        clippy::borrowed_box,
        reason = "serde's skip_serializing_if hands the boxed field over by reference"
    )]
    fn is_empty_boxed(overrides: &Box<Self>) -> bool {
        overrides.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.reasoning.is_none()
            && self.generation.is_none()
            && self.protocol_turn_options.is_none()
    }

    /// Apply these overrides to `config`, the root's snapshot. An override
    /// key is resolved through `models` here, once; a reasoning override
    /// applies to whichever model the root ends up with. An override of
    /// either has the pair the root would record judged against the recorded
    /// capability. The protocol options are applied apart, by their owner
    /// ([`apply_protocol_options`]).
    fn apply(
        &self,
        config: &mut PersistedSessionConfig,
        models: &dyn crate::provider::RuntimeModels,
    ) -> Result<(), RunResolveError> {
        if let Some(key) = &self.model {
            let recorded = models.snapshot(key).map_err(RunResolveError::Model)?;
            let reasoning = config
                .model
                .as_ref()
                .map(|current| current.reasoning.clone())
                .unwrap_or_default();
            config.model = Some(crate::ModelConfig {
                model: recorded,
                reasoning,
            });
        }
        if let Some(reasoning) = &self.reasoning {
            let model = config
                .model
                .as_mut()
                .ok_or(RunShapeRefusal::ReasoningWithoutModel)?;
            model.reasoning = reasoning.clone();
        }
        if (self.model.is_some() || self.reasoning.is_some())
            && let Some(model) = config.model.as_ref()
        {
            model
                .validate_reasoning()
                .map_err(|refusal| RunShapeRefusal::Reasoning { refusal })?;
        }
        if let Some(generation) = &self.generation {
            config.generation = generation.clone();
        }
        Ok(())
    }
}

/// Lay one statement of a run's protocol `options` over the protocol
/// namespace `config` holds, through its `owner`.
fn apply_protocol_options(
    config: &mut PersistedSessionConfig,
    options: &ProtocolTurnOptions,
    owner: &dyn RunOptionsOwner,
) -> Result<(), RunResolveError> {
    let Some(protocol) = config.plugin_config.protocol_plugin_id() else {
        return Err(RunShapeRefusal::ProtocolOptionsWithoutProtocol.into());
    };
    let protocol = protocol.to_string();
    let applied = owner
        .apply_run_options(&config.plugin_config, &protocol, options)
        .map_err(|fault| match fault {
            crate::config_transaction::ConfigFault::Refused(refusal) => {
                RunResolveError::Refused(RunShapeRefusal::Owner { refusal })
            }
            crate::config_transaction::ConfigFault::RecordedCorrupt(corrupt) => {
                RunResolveError::RecordedCorrupt(corrupt)
            }
        })?;
    config.plugin_config.insert(protocol, applied);
    Ok(())
}

/// The owner of the session's protocol namespace, as a spec's resolution
/// meets it: the one party that lays a run's stated options over the
/// recorded namespace. The session's config registry implements it.
pub trait RunOptionsOwner: Send + Sync {
    /// `config`'s `protocol` namespace with the run's `options` applied.
    fn apply_run_options(
        &self,
        config: &crate::PluginConfig,
        protocol: &str,
        options: &ProtocolTurnOptions,
    ) -> Result<serde_json::Value, crate::config_transaction::ConfigFault>;
}

/// The owners of a session whose deployment registers none: protocol options
/// a run states name an owner nobody registers, and are refused as such.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRunOptionsOwner;

impl RunOptionsOwner for NoRunOptionsOwner {
    fn apply_run_options(
        &self,
        _config: &crate::PluginConfig,
        protocol: &str,
        _options: &ProtocolTurnOptions,
    ) -> Result<serde_json::Value, crate::config_transaction::ConfigFault> {
        Err(crate::config_transaction::ConfigRefusal {
            owner: protocol.to_string(),
            at: crate::config_transaction::RefusalSite::Candidate,
            reason: crate::config_transaction::ConfigRefusalReason::UnknownOwner,
        }
        .into())
    }
}

/// A refusal raised in its raiser's own type and carried as data: the
/// serialized refusal and its display text.
fn encoded_refusal<R>(refusal: &R) -> (serde_json::Value, String)
where
    R: serde::Serialize + std::fmt::Display,
{
    match serde_json::to_value(refusal) {
        Ok(encoded) => (encoded, refusal.to_string()),
        // A refusal that does not encode keeps its text, and says so.
        Err(error) => (
            serde_json::Value::Null,
            format!("{refusal} (the refusal did not encode: {error})"),
        ),
    }
}

/// Why a registered definition refused to shape a root: deterministic, so it
/// is recorded as the root's failure. `refusal` is the definition's own
/// refusal type, serialized, and `message` its display text.
#[derive(Clone, Debug, PartialEq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[error("run definition `{definition}` refused its context: {message}")]
#[serde(deny_unknown_fields)]
pub struct RunDefinitionRefusal {
    pub definition: DefinitionRef,
    pub refusal: serde_json::Value,
    pub message: String,
}

impl RunDefinitionRefusal {
    /// `definition`'s own typed `refusal`.
    pub fn new<R>(definition: DefinitionRef, refusal: &R) -> Self
    where
        R: serde::Serialize + std::fmt::Display,
    {
        let (refusal, message) = encoded_refusal(refusal);
        Self {
            definition,
            refusal,
            message,
        }
    }
}

impl Eq for RunDefinitionRefusal {}

/// Why a protocol refused to resolve the render a root's results present
/// with. `refusal` is the protocol's own refusal type, serialized, and
/// `message` its display text.
#[derive(Clone, Debug, PartialEq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[error("{message}")]
#[serde(deny_unknown_fields)]
pub struct RenderRefusal {
    pub refusal: serde_json::Value,
    pub message: String,
}

impl RenderRefusal {
    /// The protocol's own typed `refusal`.
    pub fn new<R>(refusal: &R) -> Self
    where
        R: serde::Serialize + std::fmt::Display,
    {
        let (refusal, message) = encoded_refusal(refusal);
        Self { refusal, message }
    }
}

impl Eq for RenderRefusal {}

/// Why a protocol resolved no render: it refused the root's options, or the
/// recorded namespace it reads them from is corrupt.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RenderFault {
    #[error(transparent)]
    Refused(#[from] RenderRefusal),
    #[error(transparent)]
    RecordedCorrupt(#[from] crate::config_transaction::RecordedNamespaceCorrupt),
}

/// Why a root's shape was refused: deterministic, so it is the root's
/// recorded failure, carried typed as
/// [`RuntimeErrorCause::RunShapeRefused`](crate::RuntimeErrorCause::RunShapeRefused).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunShapeRefusal {
    /// The spec's registered definition refused its context.
    #[error(transparent)]
    Definition { refusal: RunDefinitionRefusal },
    /// A config owner refused the config the run's overrides derived, or
    /// the options the run stated for it.
    #[error("the run's overrides were refused: {refusal}")]
    Owner {
        refusal: crate::config_transaction::ConfigRefusal,
    },
    /// The reasoning the root would run is one its model's recorded
    /// capability refuses.
    #[error(transparent)]
    Reasoning {
        refusal: lash_core_llm::model::ReasoningRefused,
    },
    /// The spec sets a reasoning selection for a session with no model.
    #[error("a reasoning override needs a model, and the session has selected none")]
    ReasoningWithoutModel,
    /// The spec states protocol turn options for a session that records no
    /// protocol plugin.
    #[error(
        "run overrides state protocol turn options, but the session records no protocol plugin"
    )]
    ProtocolOptionsWithoutProtocol,
    /// The protocol refused to resolve the root's render.
    #[error("the run's render was refused: {refusal}")]
    Render { refusal: RenderRefusal },
}

/// Why a spec did not resolve to a recorded shape.
#[derive(Debug, thiserror::Error)]
pub enum RunResolveError {
    /// The spec's model key has no binding on this deployment: a redeploy
    /// repairs it, so it is never the root's recorded outcome.
    #[error(transparent)]
    Model(crate::provider::ModelUnavailable),
    /// The spec is refused, on any attempt.
    #[error(transparent)]
    Refused(#[from] RunShapeRefusal),
    /// The protocol namespace the snapshot recorded does not read as its
    /// owner's type.
    #[error(transparent)]
    RecordedCorrupt(crate::config_transaction::RecordedNamespaceCorrupt),
    #[error("the spec could not be encoded: {0}")]
    Encode(#[from] serde_json::Error),
}

/// The shape one accepted input runs under.
///
/// Explicit overrides win over the definition's output, which wins over the
/// root's config snapshot. The default spec (no definition, no context, no
/// overrides) resolves to the snapshot itself.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<DefinitionRef>,
    /// Immutable data the definition reads. Request ids and tracing stay
    /// out; credentials go in only as references.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub context: serde_json::Value,
    /// Boxed so a spec rides a send cheaply when it is the default.
    #[serde(default, skip_serializing_if = "RunOverrides::is_empty_boxed")]
    pub overrides: Box<RunOverrides>,
    /// Durable capability refs, keyed by the slot they fill (D5). They are
    /// recorded data: a worker binds them by exact id, and dynamic capability
    /// calls arrive with their own journaled protocol.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub capabilities: std::collections::BTreeMap<SlotId, CapabilityRef>,
}

impl RunSpec {
    /// A spec running the registered definition `definition` over `context`.
    pub fn definition(definition: DefinitionRef, context: serde_json::Value) -> Self {
        Self {
            definition: Some(definition),
            context,
            ..Self::default()
        }
    }

    /// A spec made of `overrides` alone.
    pub fn overrides(overrides: RunOverrides) -> Self {
        Self {
            overrides: Box::new(overrides),
            ..Self::default()
        }
    }

    /// Whether this is the empty default spec, which runs under the
    /// session config and is stored as no spec at all.
    pub fn is_default(&self) -> bool {
        self.definition.is_none()
            && self.context.is_null()
            && self.overrides.is_empty()
            && self.capabilities.is_empty()
    }

    /// The canonical bytes of this spec: its serde form with object keys
    /// sorted and `-0.0` folded to `0.0`. These are the bytes a store
    /// interns, and the bytes [`hash`](Self::hash) covers.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        Ok(crate::identity_json::payload_leaf(&serde_json::to_value(
            self,
        )?))
    }

    /// The canonical text a store keeps for this spec.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        let bytes = self.canonical_bytes()?;
        String::from_utf8(bytes).map_err(|error| {
            <serde_json::Error as serde::ser::Error>::custom(format!(
                "canonical run spec bytes are not UTF-8: {error}"
            ))
        })
    }

    /// Decode a spec a store interned.
    pub fn from_canonical_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The versioned hash that names this spec: `None` for the default
    /// spec, which is never interned.
    pub fn hash(&self) -> Result<Option<RunSpecHash>, serde_json::Error> {
        if self.is_default() {
            return Ok(None);
        }
        let bytes = self.canonical_bytes()?;
        let mut identity =
            crate::stable_identity::IdentityEncoder::new("lash.run-spec", RUN_SPEC_FAMILY_VERSION);
        identity.bytes(&bytes);
        Ok(Some(RunSpecHash(crate::stable_identity::rendered_hash(
            "run-spec",
            RUN_SPEC_FAMILY_VERSION,
            &identity.finish(),
        ))))
    }

    /// Resolve this spec against `snapshot`, the root's config after the
    /// boundary's command drain. `definition` is what the spec's registered
    /// definition produced over its context (`None` without a definition).
    /// `termination` and `follow_on_recoveries` are the host's policy and
    /// follow-on recovery bound the root records.
    /// `models` mints the binding of an override key, and `owner` applies
    /// the stated protocol options: the definition's first, then the spec's
    /// own over them.
    pub fn resolve(
        &self,
        snapshot: &PersistedSessionConfig,
        definition: Option<RunOverrides>,
        termination: TerminationPolicy,
        follow_on_recoveries: u32,
        models: &dyn crate::provider::RuntimeModels,
        owner: &dyn RunOptionsOwner,
    ) -> Result<ResolvedRun, RunResolveError> {
        let mut config = snapshot.clone();
        let definition = definition.unwrap_or_default();
        let overrides = &*self.overrides;
        RunOverrides {
            model: overrides.model.clone().or(definition.model),
            reasoning: overrides.reasoning.clone().or(definition.reasoning),
            generation: overrides.generation.clone().or(definition.generation),
            protocol_turn_options: None,
        }
        .apply(&mut config, models)?;
        for options in [
            definition.protocol_turn_options.as_ref(),
            overrides.protocol_turn_options.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            apply_protocol_options(&mut config, options, owner)?;
        }
        Ok(ResolvedRun {
            spec: self.hash()?,
            resolved: (config != *snapshot).then(|| Box::new(config)),
            capabilities: self.capabilities.clone(),
            base: snapshot.clone(),
            render: None,
            termination,
            follow_on_recoveries,
        })
    }
}

/// The versioned hash that names one interned [`RunSpec`] within its
/// session: `run-spec:v<family>:blake3:<hex>`.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct RunSpecHash(String);

impl RunSpecHash {
    /// A hash read back from a store row.
    pub fn from_stored(hash: impl Into<String>) -> Self {
        Self(hash.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunSpecHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A root's recorded shape: what its spec resolved to, once.
///
/// It is the root's config record. The first execution of the root resolves
/// its spec against the snapshot and records this; every replay, redrive and
/// worker hop reads it back, so a later config command, a redeploy of the
/// definition, or a fresh worker never changes the shape a recorded root runs
/// under. Its config is the root's execution view only: commits keep writing
/// the sticky session config.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedRender {
    pub renderer_id: String,
    pub params: serde_json::Value,
}

impl RecordedRender {
    pub fn require_available<'a>(
        recorded: Option<&'a Self>,
        active_id: &str,
    ) -> Result<&'a Self, crate::RuntimeErrorCode> {
        match recorded {
            Some(recorded) if recorded.renderer_id == active_id => Ok(recorded),
            _ => Err(crate::RuntimeErrorCode::RecordedRendererUnavailable),
        }
    }
}

/// How a turn's terminal is assembled when its stream ended with neither a
/// cancellation, an explicit outcome nor a `Done` event.
///
/// A host states it; each root records the host's policy in its
/// [`ResolvedRun`] on first execution, and terminal assembly reads that
/// record. A replay, redrive or recovered follow-on on a worker with another
/// policy assembles the same terminal (ADR 0105 §1, FIG-4389).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminationPolicy {
    /// A turn that ended without `Done` stops as a runtime error with a
    /// `MissingDone` issue instead of finishing with its assistant output.
    pub treat_missing_done_as_failure: bool,
}

impl Default for TerminationPolicy {
    fn default() -> Self {
        Self {
            treat_missing_done_as_failure: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedRun {
    /// The root's snapshot: the session config after the boundary's command
    /// drain, which the spec resolved against. Its revision is the config
    /// revision the root was admitted under.
    pub base: PersistedSessionConfig,
    /// The spec the root resolved; `None` for the default spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<RunSpecHash>,
    /// The config the root runs under when its spec changed the snapshot;
    /// `None` when it runs under the snapshot itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Box<PersistedSessionConfig>>,
    /// The capability refs the spec's slots named, recorded with the shape so
    /// a replay — and a recovered follow-on, which carries this record —
    /// binds the same refs (FIG-3877).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub capabilities: std::collections::BTreeMap<SlotId, CapabilityRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RecordedRender>,
    /// The host's termination policy when the root first resolved: how its
    /// terminal is assembled when its stream ends without `Done`. Recorded so
    /// every execution of the root assembles the same terminal (FIG-4389).
    pub termination: TerminationPolicy,
    /// The host's follow-on recovery bound when the root first resolved
    /// (ADR 0101 §3): how many times a drive may recover a follow-on this
    /// root's frame switches owe before it commits failed. A pending
    /// follow-on carries this record, so every recovery of the logical run
    /// decides on it, never on the bound of the host that drives it.
    pub follow_on_recoveries: u32,
}

impl ResolvedRun {
    /// The default spec's resolution: the snapshot itself, under
    /// `termination` and `follow_on_recoveries`.
    pub fn snapshot(
        base: PersistedSessionConfig,
        termination: TerminationPolicy,
        follow_on_recoveries: u32,
    ) -> Self {
        Self {
            base,
            spec: None,
            resolved: None,
            capabilities: std::collections::BTreeMap::new(),
            render: None,
            termination,
            follow_on_recoveries,
        }
    }

    /// The config the root runs under.
    pub fn config(&self) -> &PersistedSessionConfig {
        self.resolved.as_deref().unwrap_or(&self.base)
    }
}

/// A registered run definition: a pure, deterministic function from the
/// root's config snapshot and the spec's context to overrides. It does no
/// I/O; live resources are bound on the worker by id.
pub trait RunDefinition: Send + Sync {
    fn reference(&self) -> DefinitionRef;

    fn resolve(
        &self,
        snapshot: &PersistedSessionConfig,
        context: &serde_json::Value,
    ) -> Result<RunOverrides, RunDefinitionRefusal>;
}

/// The run definitions a deployment registers, by exact reference.
///
/// A worker resolves a spec's definition only by the exact name and revision
/// the spec names. A worker without it fails the root's resolution as its
/// deployment's fault, retried and then parked, never with another revision.
#[derive(Clone, Default)]
pub struct RunDefinitions {
    by_reference: std::sync::Arc<
        std::collections::BTreeMap<DefinitionRef, std::sync::Arc<dyn RunDefinition>>,
    >,
}

impl RunDefinitions {
    /// Register `definition` under its reference, replacing an earlier
    /// registration of the same reference.
    pub fn register(&mut self, definition: std::sync::Arc<dyn RunDefinition>) {
        std::sync::Arc::make_mut(&mut self.by_reference).insert(definition.reference(), definition);
    }

    /// The definition registered under exactly `reference`.
    pub fn get(&self, reference: &DefinitionRef) -> Option<&std::sync::Arc<dyn RunDefinition>> {
        self.by_reference.get(reference)
    }

    pub fn is_empty(&self) -> bool {
        self.by_reference.is_empty()
    }
}

impl std::fmt::Debug for RunDefinitions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.by_reference.keys()).finish()
    }
}

#[cfg(test)]
#[path = "run_spec_tests.rs"]
mod tests;
