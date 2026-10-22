//! Durable plugin-namespace state carried in a session checkpoint.
use serde_json::Value;

use crate::source_map::{MapValue, SharedSourceMap};
pub use lash_core_ids::FormatVersion;
use lash_protocol_types::allocation::{AllocationBudget, AllocationError, AllocationHold};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FormatNamespace {
    State,
    Config,
}

/// A plugin cannot interpret or encode this namespace's requested format.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
#[error(
    "plugin `{plugin}` {namespace:?} format {stored} is unreadable by native format {readable}"
)]
pub struct FormatRefusal {
    pub plugin: String,
    pub namespace: FormatNamespace,
    #[schemars(with = "std::num::NonZeroU32")]
    pub stored: FormatVersion,
    #[schemars(with = "std::num::NonZeroU32")]
    pub readable: FormatVersion,
}

/// Host narrowing of an identifier's representation safety ceiling.
/// `HARD` is the format's maximum, never an allocation default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameByteLimit<const HARD: usize>(usize);
impl<const HARD: usize> NameByteLimit<HARD> {
    pub const fn standard() -> Self {
        Self(HARD)
    }
    pub fn new(max_bytes: usize) -> Option<Self> {
        (max_bytes <= HARD).then_some(Self(max_bytes))
    }
    pub const fn max_bytes(self) -> usize {
        self.0
    }
}

/// The largest encoded value a namespace key may hold.
pub const PLUGIN_STATE_VALUE_LIMIT: usize = 32 * 1024;
/// The largest encoded namespace a plugin may publish.
pub const PLUGIN_STATE_NAMESPACE_LIMIT: usize = 128 * 1024;

/// A deterministic rejection of a plugin-state key.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
pub enum KeyRejection {
    #[error("empty key")]
    Empty,
    #[error("key exceeds the configured {limit}-byte key limit")]
    TooLong { limit: usize },
    #[error("illegal byte {byte} at {at}")]
    IllegalCharacter { at: usize, byte: u8 },
}

/// A typed plugin-state failure.
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
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginStateError {
    #[error(transparent)]
    Allocation(#[from] lash_protocol_types::allocation::AllocationError),
    #[error("recorded plugin state belongs to another runtime owner")]
    EffectOwnerMismatch,
    /// A recorded resolution does not follow its namespace's applied
    /// frontier: it skips a publication, or a predecessor publishes after
    /// ownership moved on.
    #[error("plugin `{plugin}` cannot apply a recorded publication: {refusal}")]
    Frontier {
        plugin: String,
        refusal: crate::tool_run::FrontierRefusal,
    },
    /// A reduced publication was abandoned before the engine returned it, so
    /// whether it is durable is unknown: the namespace publishes nothing
    /// more until it is rebuilt from durable state.
    #[error("plugin `{plugin}` has a publication of unknown durability")]
    PublicationFenced { plugin: String },
    /// A callback returned commands where no recorded body can carry their
    /// resolution.
    #[error("plugin `{plugin}` returned state commands outside a recorded callback")]
    Unrecorded { plugin: String },
    #[error("invalid key `{key}`: {reason}")]
    InvalidKey { key: String, reason: KeyRejection },
    #[error("value `{key}` is {bytes} bytes, limit {limit}")]
    ValueTooLarge {
        key: String,
        bytes: usize,
        limit: usize,
    },
    #[error("store is {bytes} bytes, limit {limit}")]
    StoreTooLarge { bytes: usize, limit: usize },
    #[error("cannot encode `{key}`: {message}")]
    Encode { key: String, message: String },
    #[error("cannot decode `{key}`: {message}")]
    Decode { key: String, message: String },
}

impl PluginStateError {
    /// Pressure can clear when readers release their allocations. Other state
    /// failures require changed input, configuration or durable state.
    pub fn is_terminal(&self) -> bool {
        !matches!(
            self,
            Self::Allocation(AllocationError {
                kind: lash_protocol_types::allocation::ReserveError::Pressure,
                ..
            })
        )
    }
}

/// Whether `key` may name a namespace value: 1 to 128 bytes of ASCII
/// alphanumerics, `.`, `_` and `-`.
///
/// # Errors
///
/// The first [`KeyRejection`] the key meets.
pub fn validate_state_key(key: &str) -> Result<(), KeyRejection> {
    validate_state_key_with_limit(key, NameByteLimit::standard())
}
/// Validate a proposed key under deployment policy, at most the format's 128 bytes.
pub fn validate_state_key_with_limit(
    key: &str,
    limit: NameByteLimit<128>,
) -> Result<(), KeyRejection> {
    if key.is_empty() {
        return Err(KeyRejection::Empty);
    }
    if key.len() > limit.max_bytes() {
        return Err(KeyRejection::TooLong {
            limit: limit.max_bytes(),
        });
    }
    match key
        .bytes()
        .enumerate()
        .find(|(_, byte)| !byte.is_ascii_alphanumeric() && !b"._-".contains(byte))
    {
        Some((at, byte)) => Err(KeyRejection::IllegalCharacter { at, byte }),
        None => Ok(()),
    }
}

/// The format stamp travels with a config namespace through every carrier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginConfigNamespace {
    #[schemars(with = "std::num::NonZeroU32")]
    pub format_version: FormatVersion,
    // Immutable canonical JSON, shared by admitted snapshots and their views.
    pub value: ConfigJson,
}

/// A namespace format refusal or a refusal to retain its materialized config.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
pub enum ConfigAdmissionError {
    #[error(transparent)]
    Format(#[from] FormatRefusal),
    #[error(transparent)]
    Allocation(#[from] lash_protocol_types::allocation::AllocationError),
}
impl From<ConfigAdmissionError> for crate::runtime_error::RuntimeEffectControllerError {
    fn from(error: ConfigAdmissionError) -> Self {
        match error {
            ConfigAdmissionError::Format(error) => error.into(),
            ConfigAdmissionError::Allocation(error) => crate::StoreError::Allocation(error).into(),
        }
    }
}

impl From<ConfigAdmissionError> for crate::runtime_error::RuntimeError {
    fn from(error: ConfigAdmissionError) -> Self {
        crate::runtime_error::RuntimeEffectControllerError::from(error).into_runtime_error()
    }
}

/// Immutable namespace JSON. The reservation belongs to the same allocation
/// every namespace and protocol reader shares; its serde form is bare JSON.
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct ConfigJson(std::sync::Arc<ConfigJsonStorage>);

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
struct ConfigJsonStorage {
    value: Value,
    #[serde(skip)]
    allocation: lash_protocol_types::allocation::AllocationHold,
    #[serde(skip)]
    budget: lash_protocol_types::allocation::AllocationBudget,
}

impl ConfigJson {
    /// Independently constructed input. Serving owners adopt it with `clone_in`.
    pub fn new(value: Value) -> Self {
        Self(std::sync::Arc::new(ConfigJsonStorage {
            value,
            allocation: Default::default(),
            budget: Default::default(),
        }))
    }

    /// Admit independently supplied JSON before retaining its allocation.
    pub fn new_in(
        value: Value,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        use lash_protocol_types::allocation::SourceCapacity;
        let allocation = budget.reserve(
            value
                .owned_capacity()
                .saturating_add(std::mem::size_of::<ConfigJsonStorage>() as u64 + 64),
        )?;
        Ok(Self(std::sync::Arc::new(ConfigJsonStorage {
            value,
            allocation,
            budget: budget.clone(),
        })))
    }

    /// Reserve the full JSON allocation before copying foreign source data.
    pub fn clone_in(
        &self,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        use lash_protocol_types::allocation::SourceCapacity;
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        let allocation = budget.reserve(
            self.0
                .value
                .owned_capacity()
                .saturating_add(std::mem::size_of::<ConfigJsonStorage>() as u64 + 64),
        )?;
        Ok(Self(std::sync::Arc::new(ConfigJsonStorage {
            value: self.0.value.clone(),
            allocation,
            budget: budget.clone(),
        })))
    }

    /// Transfer a source decode's reservation before exposing any readers.
    pub(crate) fn decoded_in(
        &mut self,
        allocation: lash_protocol_types::allocation::AllocationHold,
        budget: lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<(), crate::StoreError> {
        let storage = std::sync::Arc::get_mut(&mut self.0).ok_or_else(|| {
            crate::StoreError::StoredDataCorrupt {
                record_kind: "PluginConfig",
                message: "a fresh decode unexpectedly shares namespace JSON before admission"
                    .into(),
            }
        })?;
        storage.allocation = allocation;
        storage.budget = budget;
        Ok(())
    }
}
impl From<Value> for ConfigJson {
    fn from(value: Value) -> Self {
        Self::new(value)
    }
}
impl AsRef<Value> for ConfigJson {
    fn as_ref(&self) -> &Value {
        &self.0.value
    }
}
impl std::ops::Deref for ConfigJson {
    type Target = Value;
    fn deref(&self) -> &Value {
        self.as_ref()
    }
}
impl PartialEq for ConfigJson {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0) || self.0.value == other.0.value
    }
}
impl Eq for ConfigJson {}

/// How a fork treats a plugin namespace: what the plugin declares its state
/// means (D-SESSIONLAW), recorded with the namespace so a fork made after a
/// restart follows the same rule. No code runs at fork time.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StateFork {
    /// The child gets the namespace exactly as of the fork point.
    #[default]
    Copy,
    /// The child starts from the plugin's initial state, as a fresh session
    /// does; the plugin mints any external resource lazily when it next
    /// needs one.
    Reset,
}

/// Every plugin namespace of one owner, as it is resident: what a head
/// records through its namespace map and per-namespace bodies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginState {
    pub plugins: SharedSourceMap<String, PluginNamespaceState>,
}

/// One namespace's format, mediated generation, fork policy and JSON values.
/// The values are immutable and shared: a clone, a prompt cut or a
/// publication candidate references them, and a change replaces them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginNamespaceState {
    pub format_version: FormatVersion,
    /// Freshness of the values, including format conversions.
    pub generation: u64,
    /// Publication ownership and durable dedup evidence, carried with the values.
    pub publication: crate::tool_run::StateFrontier,
    /// What a fork of the owner does with this namespace.
    pub fork: StateFork,
    pub values: NamespaceValues,
    /// Resident encoding cache; never part of the namespace's stored value.
    #[serde(skip)]
    pub encoding: NamespaceEncoding,
}

/// A namespace's values, shared immutably.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NamespaceValues(std::sync::Arc<NamespaceMap>);

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct NamespaceMap {
    values: BTreeMap<String, Value>,
    #[serde(skip)]
    allocation: lash_protocol_types::allocation::AllocationHold,
    #[serde(skip)]
    budget: lash_protocol_types::allocation::AllocationBudget,
}

/// A non-owning identity of the immutable namespace source.
#[derive(Clone, Debug)]
pub struct NamespaceValuesWeak {
    source: std::sync::Weak<NamespaceMap>,
    admission: std::sync::Arc<WeakAdmission>,
}
#[derive(Debug)]
struct WeakAdmission {
    _allocation: AllocationHold,
    budget: AllocationBudget,
}
impl NamespaceValuesWeak {
    pub fn matches(&self, values: &NamespaceValues) -> bool {
        std::ptr::eq(self.source.as_ptr(), std::sync::Arc::as_ptr(&values.0))
    }
    pub fn upgrade(&self) -> Option<NamespaceValues> {
        self.source.upgrade().map(NamespaceValues)
    }
}

impl NamespaceValues {
    pub fn empty_in(budget: &AllocationBudget) -> Result<Self, AllocationError> {
        let allocation = budget.reserve(256)?;
        Ok(Self(std::sync::Arc::new(NamespaceMap {
            values: BTreeMap::new(),
            allocation,
            budget: budget.clone(),
        })))
    }
    pub(crate) fn from_admitted(
        values: BTreeMap<String, Value>,
        allocation: AllocationHold,
        budget: AllocationBudget,
    ) -> Self {
        Self(std::sync::Arc::new(NamespaceMap {
            values,
            allocation,
            budget,
        }))
    }

    pub fn allocation_budget(&self) -> &lash_protocol_types::allocation::AllocationBudget {
        &self.0.budget
    }
    pub fn downgrade(&self) -> Result<NamespaceValuesWeak, AllocationError> {
        let allocation = self.0.budget.reserve(256)?;
        Ok(NamespaceValuesWeak {
            source: std::sync::Arc::downgrade(&self.0),
            admission: std::sync::Arc::new(WeakAdmission {
                _allocation: allocation,
                budget: self.0.budget.clone(),
            }),
        })
    }
    pub(crate) fn decoded_in(
        &mut self,
        allocation: AllocationHold,
        budget: AllocationBudget,
    ) -> bool {
        let Some(storage) = std::sync::Arc::get_mut(&mut self.0) else {
            return false;
        };
        storage.allocation = allocation;
        storage.budget = budget;
        true
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn clone_in(
        &self,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        let allocation = budget.reserve(self.capacity())?;
        Ok(Self(std::sync::Arc::new(NamespaceMap {
            values: self.0.values.clone(),
            allocation,
            budget: budget.clone(),
        })))
    }
    pub fn copy_from_map_in(
        values: &BTreeMap<String, Value>,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        let allocation = budget.reserve(Self::map_capacity(values))?;
        Ok(Self(std::sync::Arc::new(NamespaceMap {
            values: values.clone(),
            allocation,
            budget: budget.clone(),
        })))
    }
    fn capacity(&self) -> u64 {
        Self::map_capacity(&self.0.values)
    }
    fn map_capacity(values: &BTreeMap<String, Value>) -> u64 {
        use lash_protocol_types::allocation::SourceCapacity;
        values.iter().fold(256u64, |bytes, (key, value)| {
            bytes
                .saturating_add(2048)
                .saturating_add(key.owned_capacity())
                .saturating_add(value.owned_capacity())
        })
    }
    pub fn changed(
        &self,
        changes: &[crate::tool_run::ResolvedStateChange],
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        use lash_protocol_types::allocation::SourceCapacity;
        let bytes = changes
            .iter()
            .fold(self.capacity(), |bytes, change| match change {
                crate::tool_run::ResolvedStateChange::Put { key, value } => bytes
                    .saturating_add(2048)
                    .saturating_add(key.owned_capacity())
                    .saturating_add(value.owned_capacity()),
                crate::tool_run::ResolvedStateChange::Delete { .. } => bytes,
            });
        let allocation = self.0.budget.reserve(bytes)?;
        let mut values = self.0.values.clone();
        for change in changes {
            change.apply_to(&mut values);
        }
        Ok(Self(std::sync::Arc::new(NamespaceMap {
            values,
            allocation,
            budget: self.0.budget.clone(),
        })))
    }
}
impl From<BTreeMap<String, Value>> for NamespaceValues {
    fn from(values: BTreeMap<String, Value>) -> Self {
        Self(std::sync::Arc::new(NamespaceMap {
            values,
            ..Default::default()
        }))
    }
}
impl std::ops::Deref for NamespaceValues {
    type Target = BTreeMap<String, Value>;
    fn deref(&self) -> &Self::Target {
        &self.0.values
    }
}
impl PartialEq for NamespaceValues {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || self.0.values == other.0.values
    }
}
impl Eq for NamespaceValues {}

impl PluginState {
    pub fn empty_in(budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(Self {
            plugins: SharedSourceMap::empty_in(budget)?,
        })
    }
    pub fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(Self {
            plugins: self.plugins.clone_in(budget)?,
        })
    }
    pub fn allocation_budget(&self) -> &AllocationBudget {
        self.plugins.allocation_budget()
    }
    pub fn insert_namespace(
        &mut self,
        plugin: &str,
        namespace: PluginNamespaceState,
    ) -> Result<(), AllocationError> {
        let _key = self.allocation_budget().reserve(plugin.len() as u64 + 64)?;
        self.plugins.insert(plugin.to_owned(), namespace)?;
        Ok(())
    }
    pub fn decoded_in(&mut self, allocation: AllocationHold, budget: AllocationBudget) -> bool {
        self.plugins
            .decoded_in_with(allocation, budget, PluginNamespaceState::decoded_in)
    }
}
impl PluginStateMap {
    pub fn decoded_in(&mut self, allocation: AllocationHold, budget: AllocationBudget) -> bool {
        self.plugins
            .decoded_in_with(allocation, budget, NamespaceEntry::decoded_in)
    }
    pub fn empty_in(budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(Self {
            plugins: SharedSourceMap::empty_in(budget)?,
        })
    }
    pub fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(Self {
            plugins: self.plugins.clone_in(budget)?,
        })
    }
}
impl MapValue for PluginNamespaceState {
    fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(Self {
            format_version: self.format_version,
            generation: self.generation,
            publication: self.publication.clone_in(budget)?,
            fork: self.fork,
            values: self.values.clone_in(budget)?,
            encoding: self.encoding.clone_in(budget)?,
        })
    }
}
impl MapValue for NamespaceEntry {
    fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(Self {
            format_version: self.format_version,
            generation: self.generation,
            publication: self.publication.clone_in(budget)?,
            fork: self.fork,
            values: self.values.clone_in(budget)?,
        })
    }
}
impl MapValue for NamespaceBody {
    fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Self::clone_in(self, budget)
    }
}
impl MapValue for NamespaceValuesWeak {
    fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        if self.admission.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        let allocation = budget.reserve(256)?;
        Ok(Self {
            source: self.source.clone(),
            admission: std::sync::Arc::new(WeakAdmission {
                _allocation: allocation,
                budget: budget.clone(),
            }),
        })
    }
}

impl Default for PluginNamespaceState {
    fn default() -> Self {
        Self {
            format_version: FormatVersion::ONE,
            generation: 0,
            publication: Default::default(),
            fork: StateFork::Copy,
            values: NamespaceValues::default(),
            encoding: NamespaceEncoding::default(),
        }
    }
}

impl PluginNamespaceState {
    pub(crate) fn decoded_in(
        &mut self,
        allocation: AllocationHold,
        budget: AllocationBudget,
    ) -> bool {
        self.values.decoded_in(allocation.clone(), budget.clone())
            && self
                .publication
                .decoded_in(allocation.clone(), budget.clone())
            && self.encoding.decoded_in(allocation, budget)
    }
    pub fn empty_in(budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Ok(Self {
            format_version: FormatVersion::ONE,
            generation: 0,
            publication: crate::tool_run::StateFrontier::empty_in(budget)?,
            fork: StateFork::Copy,
            values: NamespaceValues::empty_in(budget)?,
            encoding: NamespaceEncoding::empty_in(budget)?,
        })
    }
    pub fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        MapValue::clone_in(self, budget)
    }

    /// The persistent keyed graph of these values. Unchanged values reuse
    /// their encoded nodes; an edit encodes only its value and trie path.
    pub fn body(&self) -> Result<NamespaceBody, lash_protocol_types::allocation::AllocationError> {
        self.encoding.body(&self.values)
    }

    /// Replace the immutable source only after its complete growth is admitted.
    pub fn apply_changes(
        &mut self,
        changes: &[crate::tool_run::ResolvedStateChange],
    ) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        let values = self.values.changed(changes)?;
        self.replace_values(values)
    }

    /// Retain a foreign source only after reserving its copy on this ledger.
    pub fn replace_values(
        &mut self,
        values: NamespaceValues,
    ) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        let values = values.clone_in(self.values.allocation_budget())?;
        self.encoding = self.encoding.detached(self.values.allocation_budget())?;
        self.values = values;
        Ok(())
    }

    /// The namespace's entry with its values stored at `values`.
    pub fn entry(&self, values: &crate::store::BlobRef) -> Result<NamespaceEntry, AllocationError> {
        Ok(NamespaceEntry {
            values: crate::store::SharedCheckpointAddress::copy_from_ref_in(
                values,
                self.values.allocation_budget(),
            )?,
            format_version: self.format_version,
            generation: self.generation,
            publication: self.publication.clone(),
            fork: self.fork,
        })
    }

    /// Whether `entry` records this namespace's metadata. Equal generations
    /// within one owner's lineage mean equal values: every change of the
    /// values advances the generation.
    #[must_use]
    pub fn is_recorded_by(&self, entry: &NamespaceEntry) -> bool {
        self.generation == entry.generation
            && self.format_version == entry.format_version
            && self.fork == entry.fork
            && self.publication == entry.publication
    }
}

/// One namespace as a head or a run records it: its values body by content
/// address, and its metadata beside it, so a refusal or an owner change
/// never rehashes unchanged values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceEntry {
    /// The content address of the namespace's values body.
    pub values: crate::store::SharedCheckpointAddress,
    pub format_version: FormatVersion,
    pub generation: u64,
    pub publication: crate::tool_run::StateFrontier,
    pub fork: StateFork,
}

impl NamespaceEntry {
    fn decoded_in(&mut self, allocation: AllocationHold, budget: AllocationBudget) -> bool {
        self.values.decoded_in(allocation.clone(), budget.clone())
            && self.publication.decoded_in(allocation, budget)
    }
    /// The namespace this entry records, with `values` its body decoded.
    pub fn namespace(
        &self,
        values: NamespaceValues,
    ) -> Result<PluginNamespaceState, AllocationError> {
        Ok(PluginNamespaceState {
            format_version: self.format_version,
            generation: self.generation,
            publication: self.publication.clone_in(values.allocation_budget())?,
            fork: self.fork,
            encoding: NamespaceEncoding::default().clone_in(values.allocation_budget())?,
            values,
        })
    }
}

/// A head's namespace map: every namespace's entry, by plugin. The values
/// bodies are separate components, one per namespace
/// ([`namespace_component_key`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginStateMap {
    pub plugins: SharedSourceMap<String, NamespaceEntry>,
}

const NAMESPACE_COMPONENT_PREFIX: &str = "plugin_state/";

/// The checkpoint component key of `plugin`'s values body.
#[must_use]
pub fn namespace_component_key(plugin: &str) -> String {
    format!("{NAMESPACE_COMPONENT_PREFIX}{plugin}")
}

/// The plugin a values-body component key names, if it names one.
#[must_use]
pub fn namespace_component_plugin(key: &str) -> Option<&str> {
    key.strip_prefix(NAMESPACE_COMPONENT_PREFIX)
}

mod namespace_map;
pub use namespace_map::{NamespaceBody, NamespaceEncoding};

/// Apply each namespace's declared fork policy to `checkpoint`, a catalog
/// fork's copy of its source revision (D-SESSIONLAW): a `copy` namespace
/// keeps its entry, its values body shared by content address, with a fresh
/// publication owner segment; a `reset` namespace leaves the child's map
/// and its body with it, so the child starts it from the plugin's initial
/// state as a fresh session does. Answers whether the checkpoint changed.
///
/// # Errors
///
/// [`crate::StoreError`] when the namespace map does not decode or encode.
pub fn fork_checkpoint_plugin_state(
    checkpoint: &mut crate::store::HydratedSessionCheckpoint,
    fleet_format: crate::store::FleetFormat,
) -> Result<bool, crate::StoreError> {
    let key = crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT;
    let Some(map) = checkpoint.decode_component_for_fleet::<PluginStateMap>(key, fleet_format)?
    else {
        return Ok(false);
    };
    let mut forked = PluginStateMap::default();
    for (plugin, entry) in &map.plugins {
        let mut entry = entry.clone();
        match entry.fork {
            StateFork::Copy => {
                entry.publication.owner_segment = Default::default();
                forked.plugins.insert(plugin.clone(), entry)?;
            }
            StateFork::Reset => {
                checkpoint
                    .components
                    .remove(&namespace_component_key(plugin));
            }
        }
    }
    // A catalog fork is in the source store: its copied roots already exist.
    // The hydrated closure is a read view, not a new publication.
    checkpoint.namespace_nodes.clear();
    if forked == map {
        return Ok(false);
    }
    let body = crate::store::encode_checkpoint_component(
        key,
        &forked,
        &lash_protocol_types::allocation::AllocationBudget::unowned(),
    )?;
    checkpoint.components.insert(
        key.to_owned(),
        crate::store::HydratedCheckpointComponent::changed_for_fleet(body, fleet_format),
    );
    Ok(true)
}

/// The largest encoded total a session's publication may commit: every
/// namespace's values and its publication frontier.
pub const PLUGIN_STATE_SESSION_LIMIT: usize = 6 * 1024 * 1024;
/// The encoded total past which a committed publication is reported by
/// default: the warn tier of [`PLUGIN_STATE_SESSION_LIMIT`], and the standard
/// `DiagnosticLimits::plugin_state_warn_bytes`.
pub const PLUGIN_STATE_SESSION_WARN: usize = 4 * 1024 * 1024;

impl From<FormatRefusal> for crate::runtime_error::RuntimeEffectControllerError {
    fn from(refusal: FormatRefusal) -> Self {
        let mut error = Self::new(
            crate::runtime_error::RuntimeErrorCode::Plugin,
            refusal.to_string(),
        );
        error.cause = Some(crate::runtime_error::RuntimeErrorCause::PluginFormat {
            refusal: Box::new(refusal),
        });
        error
    }
}

impl From<FormatRefusal> for crate::runtime_error::RuntimeError {
    fn from(refusal: FormatRefusal) -> Self {
        crate::runtime_error::RuntimeEffectControllerError::from(refusal).into_runtime_error()
    }
}

impl crate::store::DurableRecord for PluginState {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::CURRENT_SESSION_STATE_VERSION);
}

#[cfg(all(test, feature = "perf-witness"))]
mod keyed_publication_tests {
    use super::*;
    use lash_checkpoint_encoder::perf_witness as work;
    use std::sync::atomic::Ordering;

    /// PLUGIN-KEY-LOCAL-PUBLICATION: encoding and hashing a one-byte edit
    /// depends on the changed key and index path, never the unchanged payload.
    #[test]
    fn plugin_key_local_publication() -> Result<(), Box<dyn std::error::Error>> {
        let measure = |size| -> Result<_, Box<dyn std::error::Error>> {
            let values = BTreeMap::from([
                ("a".to_owned(), Value::String("0".into())),
                ("z".to_owned(), Value::String("x".repeat(size))),
            ]);
            let mut namespace = PluginNamespaceState {
                values: values.into(),
                ..PluginNamespaceState::default()
            };
            let before = namespace.body()?;
            namespace.apply_changes(&[crate::tool_run::ResolvedStateChange::Put {
                key: "a".into(),
                value: Value::String("1".into()),
            }])?;
            work::reset();
            work::COLLECTOR_STATE.store(2, Ordering::Relaxed);
            let after = namespace.body()?;
            work::COLLECTOR_STATE.store(0, Ordering::Relaxed);
            let counts = (
                work::GRAPH_SERIALIZED_BODY_BYTES.load(Ordering::Relaxed),
                work::HASHED_BYTES.load(Ordering::Relaxed),
            );
            assert!(before.value_address("z").is_some());
            assert_eq!(before.value_address("z"), after.value_address("z"));
            let publication = after.publication_after(Some(&before), true)?;
            let publication_bytes = publication
                .published()
                .values()
                .map(|node| node.bytes.len())
                .sum::<usize>();
            let mut installed = before.graph().clone();
            installed.extend(publication.published().clone());
            let reopened = NamespaceBody::load(
                &installed,
                after.root(),
                crate::checkpoint_graph::DecodeLimits::within_total(1 << 28),
            )?;
            assert_eq!(reopened.snapshot(), namespace.values);
            assert!(publication.published().len() <= 3);
            let key = namespace_component_key("counted");
            let checkpoint = crate::store::HydratedSessionCheckpoint {
                components: BTreeMap::from([(
                    key.clone(),
                    crate::store::HydratedCheckpointComponent::changed(after.bytes().clone()),
                )]),
                namespace_nodes: after.graph().clone(),
                ..Default::default()
            };
            assert_eq!(
                checkpoint.decode_component::<BTreeMap<String, Value>>(&key)?,
                Some((*namespace.values).clone())
            );
            namespace.apply_changes(&[crate::tool_run::ResolvedStateChange::Put {
                key: "a".into(),
                value: Value::String("0".into()),
            }])?;
            assert_eq!(namespace.body()?.values(), before.values());
            namespace.replace_values(BTreeMap::new().into())?;
            let empty = namespace.body()?;
            assert_eq!(
                empty.values(),
                NamespaceBody::encode(
                    &BTreeMap::new(),
                    &lash_protocol_types::allocation::AllocationBudget::unowned()
                )
                .unwrap()
                .values()
            );
            Ok((
                counts.0,
                counts.1,
                publication.published().len(),
                publication_bytes,
            ))
        };
        let small = measure(1024)?;
        let large = measure(1024 * 1024)?;
        eprintln!("PLUGIN-KEY-LOCAL-PUBLICATION: 1KiB={small:?}, 1MiB={large:?}");
        assert!(large.0 <= small.0 + 1024, "unchanged values were encoded");
        assert!(large.1 <= small.1 + 1024, "unchanged values were hashed");
        assert_eq!((small.2, small.3), (large.2, large.3));
        Ok(())
    }
}

#[cfg(test)]
mod namespace_retention_tests;
