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

/// Complete plugin-state checkpoint body, including non-resident namespaces.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginState {
    pub plugins: BTreeMap<String, PluginNamespaceState>,
}
/// One namespace's format, mediated generation and JSON values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginNamespaceState {
    pub format_version: FormatVersion,
    /// Freshness of the values, including format conversions.
    pub generation: u64,
    /// Publication ownership and durable dedup evidence, carried with the values.
    pub publication: crate::tool_run::StateFrontier,
    pub values: BTreeMap<String, Value>,
}

impl Default for PluginNamespaceState {
    fn default() -> Self {
        Self {
            format_version: FormatVersion::ONE,
            generation: 0,
            publication: Default::default(),
            values: BTreeMap::new(),
        }
    }
}

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
