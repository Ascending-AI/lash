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

use std::collections::HashMap;

use crate::session_graph::PersistedSessionConfig;
use crate::{GenerationOptions, ModelSpec, PromptLayer, ProtocolTurnOptions};

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
    /// A prompt layer stacked on the session's prompt with the usual
    /// assembly precedence: its template replaces, a reset slot replaces
    /// that slot, and other contributions add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptLayer>,
    /// The provider route id. A provider-only override keeps the snapshot's
    /// model and variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationOptions>,
    /// Protocol-owned turn options (RLM finish policy and schema included),
    /// merged over the snapshot's options key by key.
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
        self.prompt.is_none()
            && self.provider_id.is_none()
            && self.model.is_none()
            && self.generation.is_none()
            && self.protocol_turn_options.is_none()
    }

    /// `self` over `under`: every field `self` sets wins.
    #[must_use]
    pub fn over(self, under: Self) -> Self {
        Self {
            prompt: match (under.prompt, self.prompt) {
                (Some(mut under), Some(top)) => {
                    stack_prompt_layer(&mut under, &top);
                    Some(under)
                }
                (under, top) => top.or(under),
            },
            provider_id: self.provider_id.or(under.provider_id),
            model: self.model.or(under.model),
            generation: self.generation.or(under.generation),
            protocol_turn_options: match (under.protocol_turn_options, self.protocol_turn_options) {
                (Some(under), Some(top)) => Some(under.merged_with(&top)),
                (under, top) => top.or(under),
            },
        }
    }

    /// Apply these overrides to `config`, the root's snapshot.
    fn apply(&self, config: &mut PersistedSessionConfig) {
        if let Some(prompt) = &self.prompt {
            let mut stacked = config.prompt.clone().unwrap_or_default();
            stack_prompt_layer(&mut stacked, prompt);
            config.prompt = Some(stacked);
        }
        if let Some(provider_id) = &self.provider_id {
            config.provider_id.clone_from(provider_id);
        }
        if let Some(model) = &self.model {
            config.model = model.clone();
        }
        if let Some(generation) = &self.generation {
            config.generation = generation.clone();
        }
        if let Some(options) = &self.protocol_turn_options {
            config.protocol_turn_options = Some(match &config.protocol_turn_options {
                Some(base) => base.merged_with(options),
                None => options.clone(),
            });
        }
    }
}

/// Stack `top` on `base` so that resolving `[base]` equals resolving
/// `[base, top]`: a template replaces, a reset slot replaces the slot, and
/// other contributions add after the base's.
fn stack_prompt_layer(base: &mut PromptLayer, top: &PromptLayer) {
    if let Some(template) = &top.template {
        base.template = Some(template.clone());
    }
    let slots: &HashMap<_, _> = &top.slots;
    for (slot, layer) in slots {
        if layer.reset {
            base.slots.insert(*slot, layer.clone());
        } else {
            base.slots
                .entry(*slot)
                .or_default()
                .contributions
                .extend(layer.contributions.iter().cloned());
        }
    }
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
    pub fn resolve(
        &self,
        snapshot: &PersistedSessionConfig,
        definition: Option<RunOverrides>,
    ) -> Result<ResolvedRun, serde_json::Error> {
        let mut config = snapshot.clone();
        let overrides = (*self.overrides)
            .clone()
            .over(definition.unwrap_or_default());
        overrides.apply(&mut config);
        Ok(ResolvedRun {
            spec: self.hash()?,
            resolved: (config != *snapshot).then(|| Box::new(config)),
            capabilities: self.capabilities.clone(),
            base: snapshot.clone(),
            render: None,
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
}

impl ResolvedRun {
    /// The default spec's resolution: the snapshot itself.
    pub fn snapshot(base: PersistedSessionConfig) -> Self {
        Self {
            base,
            spec: None,
            resolved: None,
            capabilities: std::collections::BTreeMap::new(),
            render: None,
        }
    }

    /// The config the root runs under.
    pub fn config(&self) -> &PersistedSessionConfig {
        self.resolved.as_deref().unwrap_or(&self.base)
    }
}

/// Why a registered definition refused to shape a root: deterministic, so it
/// is recorded as the root's failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("run definition `{definition}` refused its context: {message}")]
pub struct RunShapeError {
    pub definition: DefinitionRef,
    pub message: String,
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
    ) -> Result<RunOverrides, RunShapeError>;
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
