//! Durable plugin-namespace state carried in a session checkpoint.
use serde_json::Value;

pub use lash_core_ids::FormatVersion;
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
    #[error("key exceeds 128 bytes")]
    TooLong,
    #[error("illegal byte {byte} at {at}")]
    IllegalCharacter { at: usize, byte: u8 },
}

/// Whether `key` may name a namespace value: 1 to 128 bytes of ASCII
/// alphanumerics, `.`, `_` and `-`.
///
/// # Errors
///
/// The first [`KeyRejection`] the key meets.
pub fn validate_state_key(key: &str) -> Result<(), KeyRejection> {
    if key.is_empty() {
        return Err(KeyRejection::Empty);
    }
    if key.len() > 128 {
        return Err(KeyRejection::TooLong);
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
    pub value: Value,
}

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
    pub plugins: BTreeMap<String, PluginNamespaceState>,
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
}

/// A namespace's values, shared immutably.
pub type NamespaceValues = std::sync::Arc<BTreeMap<String, Value>>;

impl Default for PluginNamespaceState {
    fn default() -> Self {
        Self {
            format_version: FormatVersion::ONE,
            generation: 0,
            publication: Default::default(),
            fork: StateFork::Copy,
            values: NamespaceValues::default(),
        }
    }
}

impl PluginNamespaceState {
    /// The values to change in place: shared values are copied first.
    pub fn values_mut(&mut self) -> &mut BTreeMap<String, Value> {
        std::sync::Arc::make_mut(&mut self.values)
    }

    /// The namespace's entry with its values stored at `values`.
    #[must_use]
    pub fn entry(&self, values: crate::store::BlobRef) -> NamespaceEntry {
        NamespaceEntry {
            values,
            format_version: self.format_version,
            generation: self.generation,
            publication: self.publication.clone(),
            fork: self.fork,
        }
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
    pub values: crate::store::BlobRef,
    pub format_version: FormatVersion,
    pub generation: u64,
    pub publication: crate::tool_run::StateFrontier,
    pub fork: StateFork,
}

impl NamespaceEntry {
    /// The namespace this entry records, with `values` its body decoded.
    #[must_use]
    pub fn namespace(&self, values: NamespaceValues) -> PluginNamespaceState {
        PluginNamespaceState {
            format_version: self.format_version,
            generation: self.generation,
            publication: self.publication.clone(),
            fork: self.fork,
            values,
        }
    }
}

/// A head's namespace map: every namespace's entry, by plugin. The values
/// bodies are separate components, one per namespace
/// ([`namespace_component_key`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginStateMap {
    pub plugins: BTreeMap<String, NamespaceEntry>,
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

/// A namespace's values body: its encoding and content address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceBody {
    pub values: crate::store::BlobRef,
    pub bytes: std::sync::Arc<[u8]>,
}

impl NamespaceBody {
    /// Encode `values`.
    #[expect(
        clippy::expect_used,
        reason = "a map of strings to `serde_json::Value` encodes in MessagePack without a failing case"
    )]
    #[must_use]
    pub fn encode(values: &BTreeMap<String, Value>) -> Self {
        let bytes: std::sync::Arc<[u8]> = rmp_serde::to_vec_named(values)
            .expect("namespace values encode")
            .into();
        Self {
            values: crate::store::BlobRef::for_content(&bytes),
            bytes,
        }
    }

    /// Decode the values `bytes` hold, checked against `values`, their
    /// recorded content address.
    ///
    /// # Errors
    ///
    /// [`crate::StoreError::StoredDataCorrupt`] when the bytes hash to
    /// another address or do not decode.
    pub fn decode(
        plugin: &str,
        values: &crate::store::BlobRef,
        bytes: &[u8],
    ) -> Result<NamespaceValues, crate::StoreError> {
        let actual = crate::store::BlobRef::for_content(bytes);
        if actual != *values {
            return Err(crate::StoreError::StoredDataCorrupt {
                record_kind: "plugin namespace body",
                message: format!(
                    "plugin `{plugin}`'s values hash to `{actual}` instead of `{values}`"
                ),
            });
        }
        rmp_serde::from_slice(bytes)
            .map(std::sync::Arc::new)
            .map_err(|error| crate::StoreError::StoredDataCorrupt {
                record_kind: "plugin namespace body",
                message: format!("plugin `{plugin}`'s values do not decode: {error}"),
            })
    }
}

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
    for (plugin, mut entry) in map.plugins.clone() {
        match entry.fork {
            StateFork::Copy => {
                entry.publication.owner_segment = Default::default();
                forked.plugins.insert(plugin, entry);
            }
            StateFork::Reset => {
                checkpoint
                    .components
                    .remove(&namespace_component_key(&plugin));
            }
        }
    }
    if forked == map {
        return Ok(false);
    }
    let body = crate::store::encode_checkpoint_component(key, &forked)?;
    checkpoint.components.insert(
        key.to_owned(),
        crate::store::HydratedCheckpointComponent::changed_for_fleet(body, fleet_format),
    );
    Ok(true)
}

/// The largest encoded total of every namespace's values a session's
/// publication may commit: below the fork and creation capture bound
/// (`SESSION_PLUGIN_INIT_MAX_BYTES`, 8 MiB), so every committed state forks.
pub const PLUGIN_STATE_SESSION_LIMIT: usize = 6 * 1024 * 1024;
/// The encoded total past which a committed publication is reported: the
/// warn tier of [`PLUGIN_STATE_SESSION_LIMIT`].
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
