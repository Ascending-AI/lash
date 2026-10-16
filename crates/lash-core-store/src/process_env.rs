//! The environment a process runs under (FIG-6028, FIG-6034).
//!
//! A process is not a session. It is the immutable pair of a document and
//! an env (FIG-6074): the document is a template performing namespaced
//! effects, and the env implements it. Whatever starts it states the whole
//! env: its model data, attachment rules, tool-call limit, the link of each
//! effect the document performs to a tool, the ordered process components
//! it is built from and the capability refs its resolutions receive. Lash
//! never infers an env, fills one by default or falls back to deployment
//! defaults: every runtime built for the process reads these bytes and
//! nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use lash_core_ids::FormatVersion;
use serde::{Deserialize, Serialize};

use crate::provider::AttachmentCapabilitySnapshot;
use crate::run_spec::{CapabilityRef, SlotId};
use crate::tool_surface::EffectLink;
use crate::{ChargeSafetyPolicy, LlmProfileConfig, MaxToolCalls, SessionId};
use lash_kernel_doc::EffectName;

use crate::process_identity::{ProcessEnvRef, process_env_ref_for_bytes};

/// Everything a process runs under besides its input and engine settings.
///
/// Immutable and content-addressed by its exact store bytes
/// ([`Self::stable_ref`]). Its component list is ordered: that order is the
/// order the process's hooks and presentation run in (D11), and it is part
/// of the address.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEnv {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<ProcessModel>,
    #[serde(
        default,
        skip_serializing_if = "crate::provider::AttachmentCapabilitySnapshot::is_empty_arc"
    )]
    attachments: Arc<AttachmentCapabilitySnapshot>,
    limits: ProcessLimits,
    /// The link of each effect the process performs, exactly: a start is
    /// refused unless these are the effects its entry calls or spawns.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    effects: BTreeMap<EffectName, EffectLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    components: Vec<ProcessComponentEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    capabilities: BTreeMap<SlotId, CapabilityRef>,
}

/// The model data a process's direct calls and tools read (D16): the data
/// form of a session policy's model fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessModel {
    profile: LlmProfileConfig,
    generation: crate::GenerationOptions,
    charge_safety: ChargeSafetyPolicy,
}

impl ProcessModel {
    pub fn new(
        profile: LlmProfileConfig,
        generation: crate::GenerationOptions,
        charge_safety: ChargeSafetyPolicy,
    ) -> Self {
        Self {
            profile,
            generation,
            charge_safety,
        }
    }

    pub fn profile(&self) -> &LlmProfileConfig {
        &self.profile
    }

    pub fn generation(&self) -> &crate::GenerationOptions {
        &self.generation
    }

    pub fn charge_safety(&self) -> &ChargeSafetyPolicy {
        &self.charge_safety
    }
}

/// The limits a process runs under. Execution budgets stay live host
/// control; only the tool-call limit is part of the environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessLimits {
    max_tool_calls: MaxToolCalls,
}

impl ProcessLimits {
    pub fn new(max_tool_calls: MaxToolCalls) -> Self {
        Self { max_tool_calls }
    }

    pub fn max_tool_calls(&self) -> MaxToolCalls {
        self.max_tool_calls
    }
}

/// The id a process component registers under. A `:` is refused: the fleet
/// writer record keys a component's config format as `component:<id>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProcessComponentId(String);

/// Why a process component id is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProcessComponentIdError {
    #[error("a process component id is empty")]
    Empty,
    #[error("process component id `{id}` contains `:`, which the fleet writer key reserves")]
    Colon { id: String },
}

impl ProcessComponentId {
    /// # Errors
    ///
    /// An empty id, or one containing `:`.
    pub fn new(id: impl Into<String>) -> Result<Self, ProcessComponentIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(ProcessComponentIdError::Empty);
        }
        if id.contains(':') {
            return Err(ProcessComponentIdError::Colon { id });
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fleet writer-record key this component's state/config format is
    /// admitted under: `component:<id>`, never a session plugin's key.
    pub fn writer_key(&self) -> String {
        format!("component:{}", self.0)
    }
}

impl TryFrom<String> for ProcessComponentId {
    type Error = ProcessComponentIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProcessComponentId> for String {
    fn from(id: ProcessComponentId) -> Self {
        id.0
    }
}

impl fmt::Display for ProcessComponentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The typed key a component crate exports for its config: the component it
/// configures and the config format it writes.
pub struct ProcessConfigKey<T> {
    component: &'static str,
    format: FormatVersion,
    _type: PhantomData<fn() -> T>,
}

impl<T> ProcessConfigKey<T> {
    pub const fn new(component: &'static str, format: FormatVersion) -> Self {
        Self {
            component,
            format,
            _type: PhantomData,
        }
    }

    pub const fn component(&self) -> &'static str {
        self.component
    }

    /// The key of the tool provider this component declares as `key`: what
    /// an env links an effect it serves to.
    pub fn tool_provider(
        &self,
        key: crate::plugin_registrations::ToolProviderKey,
    ) -> crate::tool_surface::ProviderKey {
        crate::plugin_registrations::ToolProviderHandle::new(
            crate::obligations::RegistryKind::Process,
            self.component,
            key,
        )
    }

    pub const fn format(&self) -> FormatVersion {
        self.format
    }
}

impl<T> Clone for ProcessConfigKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ProcessConfigKey<T> {}

impl<T> fmt::Debug for ProcessConfigKey<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessConfigKey")
            .field("component", &self.component)
            .field("format", &self.format)
            .finish()
    }
}

/// One component an environment names, with its config and the sessions
/// it may read through its ports (D17).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessComponentEntry {
    id: ProcessComponentId,
    config: ProcessComponentConfig,
    /// Sessions this component may read through its ports, by id. Empty
    /// unless the starter grants one.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    sessions: BTreeSet<SessionId>,
}

impl ProcessComponentEntry {
    /// The entry for `key`'s component with `config` in the key's format.
    ///
    /// # Errors
    ///
    /// The key's component id is refused, or `config` does not encode.
    pub fn new<T: Serialize>(
        key: ProcessConfigKey<T>,
        config: &T,
        sessions: impl IntoIterator<Item = SessionId>,
    ) -> Result<Self, ProcessEnvRefusal> {
        let id = ProcessComponentId::new(key.component()).map_err(|error| {
            ProcessEnvRefusal::ConfigRefused {
                id: key.component().to_owned(),
                reason: error.to_string(),
            }
        })?;
        let value =
            serde_json::to_value(config).map_err(|error| ProcessEnvRefusal::ConfigRefused {
                id: key.component().to_owned(),
                reason: format!("config does not encode: {error}"),
            })?;
        Ok(Self {
            id,
            config: ProcessComponentConfig {
                format_version: key.format(),
                value,
            },
            sessions: sessions.into_iter().collect(),
        })
    }

    /// An entry stated as raw parts: a host relaying a config it holds as
    /// data, or a fixture.
    pub fn from_parts(
        id: ProcessComponentId,
        config: ProcessComponentConfig,
        sessions: BTreeSet<SessionId>,
    ) -> Self {
        Self {
            id,
            config,
            sessions,
        }
    }

    pub fn id(&self) -> &ProcessComponentId {
        &self.id
    }

    pub fn config(&self) -> &ProcessComponentConfig {
        &self.config
    }

    pub fn sessions(&self) -> &BTreeSet<SessionId> {
        &self.sessions
    }

    /// This entry's config, decoded through `key`.
    ///
    /// # Errors
    ///
    /// The key names another component, or the value does not decode as `T`.
    pub fn decode<T: serde::de::DeserializeOwned>(
        &self,
        key: ProcessConfigKey<T>,
    ) -> Result<T, serde_json::Error> {
        if key.component() != self.id.as_str() {
            return Err(serde::de::Error::custom(format!(
                "config key for `{}` cannot decode component `{}`",
                key.component(),
                self.id
            )));
        }
        self.config.decode()
    }
}

/// A component's config as its env records it: the format it was written in
/// and the value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessComponentConfig {
    format_version: FormatVersion,
    value: serde_json::Value,
}

impl ProcessComponentConfig {
    pub fn new(format_version: FormatVersion, value: serde_json::Value) -> Self {
        Self {
            format_version,
            value,
        }
    }

    pub fn format_version(&self) -> FormatVersion {
        self.format_version
    }

    pub fn value(&self) -> &serde_json::Value {
        &self.value
    }

    /// # Errors
    ///
    /// The value does not decode as `T`.
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.value)
    }
}

/// Why a process environment is refused at admission (ADR 0004; D7, D10).
/// A refused environment stores nothing.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProcessEnvRefusal {
    #[error("{issue}")]
    PluginPreflight {
        issue: crate::plugin_registrations::PreflightIssue,
    },
    #[error("process component `{id}` is named more than once")]
    DuplicateComponent { id: String },
    #[error("process component `{id}` is not installed")]
    ComponentNotInstalled { id: String },
    /// A new env names a component the deployment declares retired
    /// (FIG-6296): only a sealed env may still name it.
    #[error("process component `{id}` is retired")]
    ComponentRetired { id: String },
    /// A sealed env links an effect to a provider of a component the
    /// deployment declares retired: every later call of it would have no
    /// body.
    #[error("process component `{id}` is retired, but the env links effect `{effect}` to it")]
    RetiredComponentServes { id: String, effect: String },
    /// A retired component published step state in the process: what its
    /// later steps would read is gone.
    #[error("process component `{id}` is retired, but the process holds its recorded state")]
    RetiredComponentHasState { id: String },
    #[error("process component `{id}` cannot read config format {format}; it reads {readable:?}")]
    ConfigFormatUnreadable {
        id: String,
        format: u32,
        readable: Vec<u32>,
    },
    #[error("process component `{id}` refused its config: {reason}")]
    ConfigRefused { id: String, reason: String },
    #[error("process component `{id}` did not build: {reason}")]
    ComponentBuildFailed { id: String, reason: String },
    #[error("process components `{first}` and `{second}` both register a presenter")]
    DuplicatePresenter { first: String, second: String },
    /// The env does not link an effect the entry calls or spawns.
    #[error("effect `{effect}` the process performs is not linked")]
    EffectNotLinked { effect: String },
    /// The env links an effect the entry never calls or spawns.
    #[error("effect `{effect}` is linked, but the process does not perform it")]
    EffectNotPerformed { effect: String },
    /// A linked tool does not fit the effect's kernel signature.
    #[error("effect `{effect}` is linked to tool `{tool}`, which does not fit it: {reason}")]
    EffectUnfit {
        effect: String,
        tool: String,
        reason: String,
    },
    /// No env component serves a linked tool under its config.
    #[error(
        "effect `{effect}` is linked to tool `{tool}`, which no env component `{provider}` serves"
    )]
    ToolNotServed {
        effect: String,
        tool: String,
        provider: String,
    },
    /// The serving component projects a linked tool differently from the
    /// surface the link retains.
    #[error(
        "effect `{effect}` is linked to tool `{tool}`, whose served surface differs from the linked one"
    )]
    LinkedSurfaceChanged { effect: String, tool: String },
    /// Two linked effect paths collide: one is a namespace of the other.
    #[error("linked effects `{first}` and `{second}` collide")]
    LinkPathCollision { first: String, second: String },
    /// Capture matched an effect to a tool of the run's recorded surface,
    /// or of the declaring cell's committed deferred match, that no
    /// component the env names serves.
    #[error("{}", capture_missing(effect, tool))]
    CaptureMissing { effect: String, tool: String },
    /// Capture matched an effect to a tool the declaring cell's deferred
    /// provider resolved, whose execution binding no component the env
    /// names reads: its codec has no process twin there, or the twin does
    /// not read the binding's format.
    #[error(
        "effect `{effect}` is linked to tool `{tool}`, whose execution binding of codec `{codec}` \
         cannot be read: {reason}"
    )]
    BindingUnreadable {
        effect: String,
        tool: String,
        codec: String,
        reason: String,
    },
    /// A restricted child env names an effect its parent does not link.
    #[error("effect `{effect}` is not linked by the parent env it is restricted from")]
    RestrictMissing { effect: String },
    /// The document's linked-effect set could not be certified.
    #[error("the process's linked effects cannot be certified: {reason}")]
    Uncertified { reason: String },
    #[error("process environment `{env_ref}` is not published")]
    Unpublished { env_ref: String },
    #[error(
        "process component `{id}` writes config format {format}, outside the fleet's permitted range"
    )]
    ConfigWriterOutsideFleetRange { id: String, format: u32 },
    #[error("{reason}")]
    Policy { reason: String },
    /// The engine refuses the engine settings the start states.
    #[error("engine `{engine}` refuses the stated settings: {reason}")]
    SettingsRefused { engine: String, reason: String },
}

impl ProcessEnv {
    /// Start an environment from the one fact no environment omits: its
    /// tool-call limit. Lash fills it from nothing.
    pub fn builder(max_tool_calls: MaxToolCalls) -> ProcessEnvBuilder {
        ProcessEnvBuilder {
            env: Self {
                model: None,
                attachments: Arc::new(AttachmentCapabilitySnapshot::default()),
                limits: ProcessLimits::new(max_tool_calls),
                effects: BTreeMap::new(),
                components: Vec::new(),
                capabilities: BTreeMap::new(),
            },
            refusal: None,
        }
    }

    /// An environment stated from `run`, the starting turn's recorded run:
    /// its model data, attachment rules, tool-call limit and capability
    /// refs, with `effects` linked and exactly `components`. Nothing is
    /// inferred from tool owners (M2).
    ///
    /// # Errors
    ///
    /// The component list is refused.
    pub fn from_run(
        run: &crate::run_spec::ResolvedRun,
        effects: BTreeMap<EffectName, EffectLink>,
        components: Vec<ProcessComponentEntry>,
    ) -> Result<ProcessEnv, ProcessEnvRefusal> {
        let config = run.config();
        let mut builder = ProcessEnv::builder(config.max_tool_calls)
            .attachments(Arc::clone(&config.attachment_acceptance))
            .effects(effects);
        if let Some(profile) = &config.model {
            builder = builder.model(ProcessModel::new(
                profile.clone(),
                config.generation.clone(),
                config.charge_safety.clone(),
            ));
        }
        for (slot, capability) in run.capabilities() {
            builder = builder.capability(slot.clone(), capability.clone());
        }
        for entry in components {
            builder = builder.entry(entry);
        }
        builder.build()
    }

    /// This env restricted to `effects`, each keeping the link this env
    /// recorded for it: a child's derived env, never re-resolved. It is a
    /// new content address.
    ///
    /// # Errors
    ///
    /// An effect this env does not link.
    pub fn restrict<'a>(
        &self,
        effects: impl IntoIterator<Item = &'a EffectName>,
    ) -> Result<ProcessEnv, ProcessEnvRefusal> {
        let mut restricted = self.clone();
        restricted.effects = effects
            .into_iter()
            .map(|effect| {
                self.effects
                    .get(effect)
                    .map(|link| (effect.clone(), link.clone()))
                    .ok_or_else(|| ProcessEnvRefusal::RestrictMissing {
                        effect: effect.to_string(),
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(restricted)
    }

    pub fn model(&self) -> Option<&ProcessModel> {
        self.model.as_ref()
    }

    pub fn attachments(&self) -> &Arc<AttachmentCapabilitySnapshot> {
        &self.attachments
    }

    pub fn limits(&self) -> ProcessLimits {
        self.limits
    }

    pub fn max_tool_calls(&self) -> MaxToolCalls {
        self.limits.max_tool_calls
    }

    /// The link of each effect the process performs.
    pub fn effects(&self) -> &BTreeMap<EffectName, EffectLink> {
        &self.effects
    }

    /// Replace only the component representations at a recorded format cut.
    #[doc(hidden)]
    pub fn with_component_entries(mut self, entries: Vec<ProcessComponentEntry>) -> Self {
        self.components = entries;
        self
    }

    /// The named components, in composition order.
    pub fn components(&self) -> &[ProcessComponentEntry] {
        &self.components
    }

    pub fn component(&self, id: &str) -> Option<&ProcessComponentEntry> {
        self.components.iter().find(|entry| entry.id.as_str() == id)
    }

    pub fn capabilities(&self) -> &BTreeMap<SlotId, CapabilityRef> {
        &self.capabilities
    }

    /// Content-addresses the exact bytes persisted by [`Self::to_store_bytes`].
    ///
    /// These bytes follow the final binary's serde-json feature set. Shapes
    /// and golden vectors change in place until the 1.0 version freeze ends.
    ///
    /// # Errors
    ///
    /// The environment does not encode.
    pub fn stable_ref(&self) -> Result<ProcessEnvRef, serde_json::Error> {
        self.to_store_bytes()
            .map(|bytes| process_env_ref_for_bytes(&bytes))
    }

    /// # Errors
    ///
    /// The environment does not encode.
    pub fn to_store_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decode stored bytes. Old shapes and unknown fields are refused.
    ///
    /// # Errors
    ///
    /// The bytes are not this build's environment.
    pub fn from_store_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// The structural refusals admission checks before any component is
    /// consulted: duplicate component ids and colliding linked effects.
    ///
    /// # Errors
    ///
    /// The first structural refusal.
    pub fn check_shape(&self) -> Result<(), ProcessEnvRefusal> {
        let paths = self
            .effects
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for (index, first) in paths.iter().enumerate() {
            for second in &paths[index + 1..] {
                let nested = |outer: &str, inner: &str| {
                    inner
                        .strip_prefix(outer)
                        .is_some_and(|rest| rest.starts_with('.'))
                };
                if nested(first, second) || nested(second, first) {
                    return Err(ProcessEnvRefusal::LinkPathCollision {
                        first: first.clone(),
                        second: second.clone(),
                    });
                }
            }
        }
        let mut ids = BTreeSet::new();
        for entry in &self.components {
            if !ids.insert(entry.id.as_str()) {
                return Err(ProcessEnvRefusal::DuplicateComponent {
                    id: entry.id.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Builds a [`ProcessEnv`]; [`Self::build`] refuses duplicate component ids.
#[derive(Debug)]
pub struct ProcessEnvBuilder {
    env: ProcessEnv,
    refusal: Option<ProcessEnvRefusal>,
}

impl ProcessEnvBuilder {
    #[must_use]
    pub fn model(mut self, model: ProcessModel) -> Self {
        self.env.model = Some(model);
        self
    }

    #[must_use]
    pub fn attachments(mut self, attachments: Arc<AttachmentCapabilitySnapshot>) -> Self {
        self.env.attachments = attachments;
        self
    }

    #[must_use]
    pub fn max_tool_calls(mut self, max_tool_calls: MaxToolCalls) -> Self {
        self.env.limits = ProcessLimits::new(max_tool_calls);
        self
    }

    /// Link `effect` to `link`, replacing an earlier link of it.
    #[must_use]
    pub fn link(mut self, effect: EffectName, link: EffectLink) -> Self {
        self.env.effects.insert(effect, link);
        self
    }

    /// Link the effect `definition`'s binding names (`module.operation`) to
    /// that tool, served by the env component provider declared as
    /// `provider`, against its process surface. A tool with no binding
    /// names no effect: [`Self::build`] refuses it.
    #[must_use]
    pub fn link_tool(
        mut self,
        provider: crate::tool_surface::ProviderKey,
        definition: &crate::ToolDefinition,
    ) -> Self {
        let link = crate::tool_surface::EffectLink {
            provider,
            surface: crate::tool_surface::ProcessSurface::of(definition),
            kind: crate::tool_surface::LinkKind::Tool,
        };
        match crate::tool_surface::effect_path_of(&definition.manifest)
            .and_then(|path| EffectName::new(path.as_str()).ok())
        {
            Some(effect) => {
                self.env.effects.insert(effect, link);
            }
            None => {
                self.refusal
                    .get_or_insert(ProcessEnvRefusal::EffectNotLinked {
                        effect: definition.manifest.name.clone(),
                    });
            }
        }
        self
    }

    /// Link exactly `effects`, replacing every earlier link.
    #[must_use]
    pub fn effects(mut self, effects: BTreeMap<EffectName, EffectLink>) -> Self {
        self.env.effects = effects;
        self
    }

    /// Append `key`'s component with `config`, after every component named
    /// before it.
    #[must_use]
    pub fn component<T: Serialize>(self, key: ProcessConfigKey<T>, config: &T) -> Self {
        self.component_with_sessions(key, config, std::iter::empty())
    }

    /// Append `key`'s component with `config`, granting it read access to
    /// `sessions` through its ports (D17).
    #[must_use]
    pub fn component_with_sessions<T: Serialize>(
        mut self,
        key: ProcessConfigKey<T>,
        config: &T,
        sessions: impl IntoIterator<Item = SessionId>,
    ) -> Self {
        match ProcessComponentEntry::new(key, config, sessions) {
            Ok(entry) => self.env.components.push(entry),
            Err(refusal) => {
                self.refusal.get_or_insert(refusal);
            }
        }
        self
    }

    /// Append an entry stated as data.
    #[must_use]
    pub fn entry(mut self, entry: ProcessComponentEntry) -> Self {
        self.env.components.push(entry);
        self
    }

    #[must_use]
    pub fn capability(mut self, slot: SlotId, capability: CapabilityRef) -> Self {
        self.env.capabilities.insert(slot, capability);
        self
    }

    /// # Errors
    ///
    /// A component config that did not encode, or a component named twice.
    pub fn build(self) -> Result<ProcessEnv, ProcessEnvRefusal> {
        if let Some(refusal) = self.refusal {
            return Err(refusal);
        }
        self.env.check_shape()?;
        Ok(self.env)
    }
}

/// Why capture found no server for `effect`'s `tool`. `processes.await` is
/// also performed by the guard a TypeScript `await` of a value of unknown
/// type lowers to, so a body that never names it can need it linked.
fn capture_missing(effect: &str, tool: &str) -> String {
    let refusal = format!(
        "effect `{effect}` matched tool `{tool}` in the run's recorded surface or the declaring cell's deferred match, which no component the env names serves"
    );
    if effect == "processes.await" {
        format!(
            "{refusal}: the body performs it explicitly or through the implicit handle-await guard \
             that every `await` of a value of unknown type lowers to; name the `processes` \
             component in the env to link it"
        )
    } else {
        refusal
    }
}

#[cfg(test)]
#[path = "process_env_tests.rs"]
mod tests;
