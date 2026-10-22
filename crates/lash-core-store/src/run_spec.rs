//! The shape an accepted input runs under (FIG-3838, D5; FIG-6021).
//!
//! Every accepted input carries a [`RunSpec`]: a registered run definition,
//! the context data that definition reads, one-shot overrides of the
//! session config, and the typed run options it states for the session's
//! plugin owners. The empty default spec is "the session config": it adds
//! nothing to an input's row or its submission digest, so an omitted spec
//! and an explicit default are the same submission.
//!
//! A non-default spec is interned per session under its [`RunSpecHash`]: the
//! input row names the hash and the session's spec table holds the canonical
//! bytes while an open input names them. The hash is part of the input's
//! submission digest, so a same-id retry with a different spec is a
//! conflict.
//!
//! A run resolves its spec once, before its first durable use, against the
//! run's config snapshot taken after the boundary's command drain, and its
//! first phase commit records the result, spec included, as the run's
//! [`ResolvedRun`] in the turn's checkpoint. A resume or a cancel of a
//! committed run reads that record and never resolves again. Overrides and
//! options shape that run only: they never reach the sticky session config.
//!
//! A claim never mixes specs: the next-turn prefix stops at the first input
//! whose spec differs from its head's. Steering joins the running run's
//! recorded shape, so an input addressed to a running turn under a differing
//! explicit spec is refused before acceptance.

use crate::session_graph::PersistedSessionConfig;
use crate::{GenerationOptions, LlmProfileKey, ReasoningSelection};

/// Family version of the [`RunSpecHash`] preimage and of the canonical spec
/// bytes it hashes.
/// version_surface = "coexist"
/// version_guard(items(RUN_SPEC_FAMILY_VERSION, hash))
pub const RUN_SPEC_FAMILY_VERSION: u8 = 1;

/// An immutable run definition name and revision a deployment registers.
///
/// Resolution looks up the exact revision; a worker that does not serve it
/// retries and parks, and never falls back to another revision.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
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

/// A slot a run's spec fills with a durable capability (D5): the name the
/// definition or the run's tooling binds the capability under.
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
///
/// A ref is recorded intent, not a checked binding (FIG-6021): the run
/// records it with its shape, and the host's deferred tool resolver receives
/// it unchanged on every resolution the run, its resumed turns and the
/// processes it starts ask for. Lash does not check before execution that
/// the worker can bind it; a ref the resolver cannot serve answers that one
/// resolution as unavailable.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CapabilityRef {
    pub contract: ContractRef,
    pub binding: BindingId,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub args: serde_json::Value,
}

// `serde_json::Value` never holds NaN or an infinite number, so its
// `PartialEq` is reflexive and `CapabilityRef` can be `Eq` — which
// `ResolvedRun`'s `Eq` requires.
impl Eq for CapabilityRef {}

/// The key a plugin crate exports for its config owner's typed run options
/// (FIG-6021): the owner's namespace, which is its plugin's id, and the
/// type that owner decodes. Registration checks the key names the
/// registering plugin, so a payload set under a plugin's own key reaches
/// that plugin's owner as its own type.
pub struct RunOptionsKey<T> {
    owner: &'static str,
    _options: std::marker::PhantomData<fn() -> T>,
}

impl<T> RunOptionsKey<T> {
    /// The key of the owner of namespace `owner`, whose run options are `T`.
    pub const fn new(owner: &'static str) -> Self {
        Self {
            owner,
            _options: std::marker::PhantomData,
        }
    }

    /// The owner's namespace.
    pub const fn owner(&self) -> &'static str {
        self.owner
    }
}

impl<T> Clone for RunOptionsKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for RunOptionsKey<T> {}

impl<T> std::fmt::Debug for RunOptionsKey<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RunOptionsKey").field(&self.owner).finish()
    }
}

/// Run options that did not encode as their owner's payload.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the run options for config owner `{owner}` do not encode: {message}")]
pub struct RunOptionsEncodeError {
    pub owner: String,
    pub message: String,
}

/// What a run states for the config owners it addresses (FIG-6021): each
/// owner's typed run options, set through that owner's [`RunOptionsKey`]
/// and carried in serialized form. Only the owner decodes and applies its
/// entry, over the namespace the run's snapshot records; nothing else
/// merges it. An absent entry inherits the namespace; a present one, empty
/// or not, is the run's statement and part of its spec's identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerRunOptions {
    entries: std::collections::BTreeMap<String, serde_json::Value>,
}

impl OwnerRunOptions {
    /// State `options` for the owner `key` names, replacing what was stated
    /// for it before.
    ///
    /// # Errors
    ///
    /// [`RunOptionsEncodeError`] when `options` does not serialize.
    pub fn set<T: serde::Serialize>(
        &mut self,
        key: RunOptionsKey<T>,
        options: T,
    ) -> Result<(), RunOptionsEncodeError> {
        let encoded = serde_json::to_value(options).map_err(|error| RunOptionsEncodeError {
            owner: key.owner.to_string(),
            message: error.to_string(),
        })?;
        self.entries.insert(key.owner.to_string(), encoded);
        Ok(())
    }

    /// These options with `options` stated for the owner `key` names.
    ///
    /// # Errors
    ///
    /// [`RunOptionsEncodeError`] when `options` does not serialize.
    pub fn with<T: serde::Serialize>(
        mut self,
        key: RunOptionsKey<T>,
        options: T,
    ) -> Result<Self, RunOptionsEncodeError> {
        self.set(key, options)?;
        Ok(self)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Each addressed owner with its serialized options, in owner order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &serde_json::Value)> {
        self.entries
            .iter()
            .map(|(owner, options)| (owner.as_str(), options))
    }

    /// The serialized options stated for `owner`.
    pub fn get(&self, owner: &str) -> Option<&serde_json::Value> {
        self.entries.get(owner)
    }
}

impl serde::Serialize for OwnerRunOptions {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.entries.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for OwnerRunOptions {
    /// A stored statement names each owner once: a duplicate key is
    /// refused, never resolved by keeping either value.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries;
        impl<'de> serde::de::Visitor<'de> for Entries {
            type Value = OwnerRunOptions;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a map of config owner ids to run options")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = std::collections::BTreeMap::new();
                while let Some((owner, options)) = map.next_entry::<String, serde_json::Value>()? {
                    if entries.contains_key(&owner) {
                        return Err(serde::de::Error::custom(format!(
                            "config owner `{owner}` is stated twice"
                        )));
                    }
                    entries.insert(owner, options);
                }
                Ok(OwnerRunOptions { entries })
            }
        }
        deserializer.deserialize_map(Entries)
    }
}

/// One-shot overrides of the session config for the run that runs an
/// input. Each field left `None` keeps the run's snapshot value.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOverrides {
    /// The model this run executes, by the host's key. The run resolves it once,
    /// when it records its shape: the registry mints the binding then, and
    /// the run's checkpoint keeps the recorded binding. The snapshot's
    /// reasoning stays unless [`Self::reasoning`] is set too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<LlmProfileKey>,
    /// The reasoning this run executes its model with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationOptions>,
    /// The typed run options this run states for the session's config
    /// owners, protocol and host plugins alike (FIG-6021). A statement for
    /// an owner the run's snapshot records no namespace for is refused.
    /// Which tools a run is offered is one such option of each tool
    /// provider's owner: it is recorded with the run's shape and never
    /// reaches the sticky config.
    #[serde(default, skip_serializing_if = "OwnerRunOptions::is_empty")]
    pub owners: OwnerRunOptions,
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
            && self.owners.is_empty()
    }

    /// Apply these overrides' core fields to `config`, the run's snapshot.
    /// An override key is resolved through `models` here, once; a reasoning
    /// override applies to whichever model the run ends up with. An
    /// override of either has the pair the run would record judged against
    /// the recorded capability. The owners' options are applied apart, by
    /// their owners ([`apply_owner_options`]).
    fn apply(
        &self,
        config: &mut PersistedSessionConfig,
        models: &dyn crate::provider::LlmProfiles,
    ) -> Result<(), RunResolveError> {
        if let Some(key) = &self.model {
            let recorded = models.snapshot(key).map_err(RunResolveError::Model)?;
            let reasoning = config
                .model
                .as_ref()
                .map(|current| current.reasoning.clone())
                .unwrap_or_default();
            config.model = Some(crate::LlmProfileConfig {
                model: recorded,
                reasoning,
            });
        }
        if let Some(reasoning) = &self.reasoning {
            let model = config
                .model
                .as_mut()
                .ok_or(RunShapeRefusal::ReasoningWithoutLlmProfile)?;
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

/// Lay one statement of a run's `options` for `owner` over the namespace
/// `config` records for it, through `owners`.
fn apply_owner_options(
    config: &mut PersistedSessionConfig,
    owner: &str,
    options: &serde_json::Value,
    owners: &dyn RunOptionsOwner,
) -> Result<(), RunResolveError> {
    let applied = owners
        .apply_run_options(&config.plugin_config, owner, options)
        .map_err(|fault| match fault {
            crate::config_transaction::ConfigFault::Allocation(error) => {
                RunResolveError::Allocation(error)
            }
            crate::config_transaction::ConfigFault::Refused(refusal) => {
                RunResolveError::Refused(RunShapeRefusal::Owner { refusal })
            }
            crate::config_transaction::ConfigFault::RecordedCorrupt(corrupt) => {
                RunResolveError::RecordedCorrupt(corrupt)
            }
        })?;
    config.plugin_config.insert(owner, applied)?;
    Ok(())
}

/// The config owners of a session, as a spec's resolution meets them: the
/// parties that each lay a run's stated options over their own recorded
/// namespace. The session's config registry implements it.
pub trait RunOptionsOwner: Send + Sync {
    /// `config`'s `owner` namespace with the run's serialized `options`
    /// applied by that namespace's owner, which alone decodes them.
    fn apply_run_options(
        &self,
        config: &crate::PluginConfig,
        owner: &str,
        options: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::config_transaction::ConfigFault>;
}

/// The owners of a session whose deployment registers none: options a run
/// states name an owner nobody registers, and are refused as such.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRunOptionsOwner;

impl RunOptionsOwner for NoRunOptionsOwner {
    fn apply_run_options(
        &self,
        _config: &crate::PluginConfig,
        owner: &str,
        _options: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::config_transaction::ConfigFault> {
        Err(crate::config_transaction::ConfigRefusal {
            owner: owner.to_string(),
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

/// Why a registered definition refused to shape a run: deterministic, so it
/// is recorded as the run's failure. `refusal` is the definition's own
/// refusal type, serialized, and `message` its display text.
#[derive(
    Clone,
    Debug,
    PartialEq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
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

/// Why a protocol refused to resolve the render a run's results present
/// with. `refusal` is the protocol's own refusal type, serialized, and
/// `message` its display text.
#[derive(
    Clone,
    Debug,
    PartialEq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
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

/// Why a protocol resolved no render: it refused the run's options, or the
/// recorded namespace it reads them from is corrupt.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RenderFault {
    #[error(transparent)]
    Refused(#[from] RenderRefusal),
    #[error(transparent)]
    RecordedCorrupt(#[from] crate::config_transaction::RecordedNamespaceCorrupt),
}

/// Why a run's shape was refused: deterministic, so it is the run's
/// recorded failure, carried typed as
/// [`RuntimeErrorCause::RunShapeRefused`](crate::RuntimeErrorCause::RunShapeRefused).
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
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
    /// The reasoning the run would run is one its model's recorded
    /// capability refuses.
    #[error(transparent)]
    Reasoning {
        refusal: lash_core_llm::llm_profile::ReasoningRefused,
    },
    /// The spec sets a reasoning selection for a session with no model.
    #[error("a reasoning override needs a model, and the session has selected none")]
    ReasoningWithoutLlmProfile,
    /// The protocol refused to resolve the run's render.
    #[error("the run's render was refused: {refusal}")]
    Render { refusal: RenderRefusal },
}

/// Why a spec did not resolve to a recorded shape.
#[derive(Debug, thiserror::Error)]
pub enum RunResolveError {
    #[error(transparent)]
    Allocation(#[from] lash_protocol_types::allocation::AllocationError),
    /// The spec's model key has no binding on this deployment: a redeploy
    /// repairs it, so it is never the run's recorded outcome.
    #[error(transparent)]
    Model(crate::provider::LlmProfileUnavailable),
    /// The spec is refused, on any attempt.
    #[error(transparent)]
    Refused(#[from] RunShapeRefusal),
    /// A namespace the snapshot recorded does not read as its owner's type.
    #[error(transparent)]
    RecordedCorrupt(crate::config_transaction::RecordedNamespaceCorrupt),
    #[error("the spec could not be encoded: {0}")]
    Encode(#[from] serde_json::Error),
}

/// The shape one accepted input runs under.
///
/// Explicit overrides win over the definition's output, which wins over the
/// run's config snapshot; an owner applies the definition's options for it
/// first, then the explicit ones. The default spec (no definition, no
/// context, no overrides) resolves to the snapshot itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    /// Observation for each cell this logical run executes. Each cell records
    /// the level it actually ran at; another run may choose another level.
    #[serde(
        default,
        skip_serializing_if = "lash_kernel_doc::RunObservation::is_off"
    )]
    pub observe: lash_kernel_doc::RunObservation,
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
    /// recorded data: a worker binds them by exact id. The run's recorded
    /// refs reach the host's deferred tool resolver with every resolution
    /// the run asks for, and a process the run starts captures them, so a
    /// host grants by the run that authorized the work.
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
        self.observe.is_off()
            && self.definition.is_none()
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

    /// Resolve this spec against `snapshot`, the run's config after the
    /// boundary's command drain. `definition` is what the spec's registered
    /// definition produced over its context (`None` without a definition).
    /// `models` mints the binding of an override key, and `owners` apply
    /// the stated run options: for each owner, the definition's first, then
    /// the spec's own over them.
    pub fn resolve(
        &self,
        snapshot: &PersistedSessionConfig,
        definition: Option<RunOverrides>,
        models: &dyn crate::provider::LlmProfiles,
        owners: &dyn RunOptionsOwner,
    ) -> Result<ResolvedRun, RunResolveError> {
        let mut config = snapshot.clone();
        let definition = definition.unwrap_or_default();
        let overrides = &*self.overrides;
        RunOverrides {
            model: overrides.model.clone().or(definition.model),
            reasoning: overrides.reasoning.clone().or(definition.reasoning),
            generation: overrides.generation.clone().or(definition.generation),
            owners: OwnerRunOptions::default(),
        }
        .apply(&mut config, models)?;
        for (owner, options) in definition.owners.iter().chain(overrides.owners.iter()) {
            apply_owner_options(&mut config, owner, options, owners)?;
        }
        Ok(ResolvedRun::snapshot(snapshot.clone())
            .with_spec(self.clone())?
            .with_config(config))
    }

    /// This spec as a frame switch carries it to its follow-on
    /// ([`NextRun`](lash_protocol_types::NextRun)).
    ///
    /// # Errors
    ///
    /// The serializer's, for a spec that does not encode.
    pub fn next_run(&self) -> Result<lash_protocol_types::NextRun, serde_json::Error> {
        Ok(lash_protocol_types::NextRun::from_encoded(
            serde_json::to_value(self)?,
        ))
    }

    /// The spec a frame switch's `next_run` carries.
    ///
    /// # Errors
    ///
    /// The deserializer's, for a carried spec that is not one.
    pub fn from_next_run(
        next_run: &lash_protocol_types::NextRun,
    ) -> Result<Self, serde_json::Error> {
        serde_json::from_value(next_run.encoded().clone())
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

/// The render a run's results present with: the protocol's renderer and its
/// resolved parameters, recorded with the run's shape.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedRender {
    pub renderer_id: String,
    /// Immutable resolved parameters, shared by every turn and call of the run.
    pub params: std::sync::Arc<serde_json::Value>,
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

/// A run's recorded shape: what its spec resolved to, once (FIG-6021).
///
/// The run's first phase commit records it in the turn's checkpoint, before
/// the turn's first durable effect. A resume, a cancel and a worker hop of
/// the committed run read it back, so a later config command, a redeploy of
/// the definition, an owner or a renderer, or a fresh worker never changes
/// the shape a recorded run executes under. Its config is the run's
/// execution view only: commits keep writing the sticky session config.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "ResolvedRunFields")]
pub struct ResolvedRun {
    /// The run's snapshot: the session config after the boundary's command
    /// drain, which the spec resolved against. Its revision is the config
    /// revision the run was admitted under.
    base: PersistedSessionConfig,
    /// The spec the run resolved, whole: the default spec for a run of
    /// wakes, a default send or a default follow-on.
    #[serde(default, skip_serializing_if = "RunSpec::is_default")]
    spec: RunSpec,
    /// The config the run executes under when its spec changed the snapshot;
    /// `None` when it runs under the snapshot itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved: Option<Box<PersistedSessionConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RecordedRender>,
    /// The hash `spec` interns under; derived, never stored.
    #[serde(skip)]
    spec_hash: Option<RunSpecHash>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedRunFields {
    base: PersistedSessionConfig,
    #[serde(default)]
    spec: RunSpec,
    #[serde(default)]
    resolved: Option<Box<PersistedSessionConfig>>,
    #[serde(default)]
    render: Option<RecordedRender>,
}

impl TryFrom<ResolvedRunFields> for ResolvedRun {
    type Error = serde_json::Error;

    fn try_from(fields: ResolvedRunFields) -> Result<Self, Self::Error> {
        let mut run = Self::snapshot(fields.base).with_spec(fields.spec)?;
        if let Some(config) = fields.resolved {
            run = run.with_config(*config);
        }
        run.render = fields.render;
        Ok(run)
    }
}

impl ResolvedRun {
    /// The default spec's resolution: the snapshot itself.
    pub fn snapshot(base: PersistedSessionConfig) -> Self {
        Self {
            base,
            spec: RunSpec::default(),
            resolved: None,
            render: None,
            spec_hash: None,
        }
    }

    /// Record `spec` as the spec this run resolved.
    ///
    /// # Errors
    ///
    /// The serializer's, for a spec that does not encode.
    pub fn with_spec(mut self, spec: RunSpec) -> Result<Self, serde_json::Error> {
        self.spec_hash = spec.hash()?;
        self.spec = spec;
        Ok(self)
    }

    /// Set the execution config, retaining an override only when it differs.
    pub fn with_config(mut self, config: PersistedSessionConfig) -> Self {
        self.resolved = (config != self.base).then(|| Box::new(config));
        self
    }

    /// The immutable snapshot the run resolved against.
    pub fn base(&self) -> &PersistedSessionConfig {
        &self.base
    }

    /// The spec the run resolved, as it was accepted.
    pub fn spec(&self) -> &RunSpec {
        &self.spec
    }

    /// The hash the run's spec interns under; `None` for the default spec.
    pub fn spec_hash(&self) -> Option<&RunSpecHash> {
        self.spec_hash.as_ref()
    }

    /// The capability refs the spec's slots named.
    pub fn capabilities(&self) -> &std::collections::BTreeMap<SlotId, CapabilityRef> {
        &self.spec.capabilities
    }

    /// Whether the run states anything beyond the session config: an
    /// override or run option that changed the snapshot.
    pub fn has_override(&self) -> bool {
        self.resolved.is_some()
    }

    /// Transform both configs and normalize again: decoding can make a
    /// previously distinct override equal to the snapshot.
    ///
    /// # Errors
    ///
    /// The transformation's error, with no partially transformed run returned.
    pub fn map_configs<E>(
        self,
        mut map: impl FnMut(PersistedSessionConfig) -> Result<PersistedSessionConfig, E>,
    ) -> Result<Self, E> {
        let base = map(self.base)?;
        let config = self.resolved.map(|config| map(*config)).transpose()?;
        let mut run = Self {
            base,
            spec: self.spec,
            resolved: None,
            render: self.render,
            spec_hash: self.spec_hash,
        };
        if let Some(config) = config {
            run = run.with_config(config);
        }
        Ok(run)
    }

    /// The config the run executes under.
    pub fn config(&self) -> &PersistedSessionConfig {
        self.resolved.as_deref().unwrap_or(&self.base)
    }
}

/// A registered run definition: a pure, deterministic function from the
/// run's config snapshot and the spec's context to overrides and owner run
/// options. It does no I/O; live resources are bound on the worker by id.
/// It runs once per run, at resolution, never on a resume.
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
/// the spec names. A worker without it fails the run's resolution as its
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
