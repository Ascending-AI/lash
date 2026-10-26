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

use lash_kernel_value::providers::FormatVersion;
use serde::{Deserialize, Serialize};

use crate::provider::AttachmentCapabilitySnapshot;
use crate::run_spec::{CapabilityRef, SlotId};
use crate::tool_surface::ExecutionEnvironment;
use crate::{ChargeSafetyPolicy, LlmProfileConfig, MaxToolCalls, SessionId};
use lash_vocabulary::claims::RegistryKind;

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
    packages: crate::tool_surface::RecordedPackages,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<ProcessModel>,
    #[serde(
        default,
        skip_serializing_if = "crate::provider::AttachmentCapabilitySnapshot::is_empty_arc"
    )]
    attachments: Arc<AttachmentCapabilitySnapshot>,
    limits: ProcessLimits,
    /// Immutable accepted links, routes and known claim requirements.
    execution: ExecutionEnvironment,
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
        key: lash_vocabulary::providers::ToolProviderKey,
    ) -> crate::tool_surface::ProviderKey {
        lash_vocabulary::providers::ToolProviderHandle::new(
            lash_vocabulary::claims::RegistryKind::Process,
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
    /// A tool linked at a path a package owns does not serve the package's
    /// operation (PKG-RUNNER).
    #[error("tool `{tool}` cannot run `{effect}`, an operation package `{package}` owns: {reason}")]
    RunnerNotPackaged {
        effect: String,
        tool: String,
        package: String,
        reason: String,
    },
    /// The serving component serves a linked tool under another contract
    /// than the process's code requires (`operation_contract_changed`).
    #[error(
        "effect `{effect}` is linked to tool `{tool}`, which serves contract `{live}`, not the \
         contract `{pinned}` the process's code requires"
    )]
    ContractChanged {
        effect: String,
        tool: String,
        pinned: lash_kernel_value::operations::ContractId,
        live: lash_kernel_value::operations::ContractId,
    },
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
                packages: Default::default(),
                model: None,
                attachments: Arc::new(AttachmentCapabilitySnapshot::default()),
                limits: ProcessLimits::new(max_tool_calls),
                execution: ExecutionEnvironment::empty(RegistryKind::Process),
                components: Vec::new(),
                capabilities: BTreeMap::new(),
            },
            refusal: None,
        }
    }

    pub fn packages(&self) -> &crate::tool_surface::RecordedPackages {
        &self.packages
    }

    pub fn with_packages(mut self, packages: crate::tool_surface::RecordedPackages) -> Self {
        self.packages = packages;
        self
    }

    pub fn execution(&self) -> &ExecutionEnvironment {
        &self.execution
    }

    /// Install owner-validated code debt while preserving authored configuration.
    pub fn with_execution(mut self, execution: ExecutionEnvironment) -> Self {
        self.execution = execution;
        self
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
    pub fn packages(mut self, packages: crate::tool_surface::RecordedPackages) -> Self {
        self.env.packages = packages;
        self
    }

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

    /// Record the selected per-use environment. Resolutions are stored separately.
    #[must_use]
    pub fn execution(mut self, execution: ExecutionEnvironment) -> Self {
        self.env.execution = execution;
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

pub use {
    lash_kernel_value::providers::ProcessComponentId,
    lash_kernel_value::providers::ProcessComponentIdError,
};
