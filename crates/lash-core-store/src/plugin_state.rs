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
    pub generation: u64,
    pub values: BTreeMap<String, Value>,
}

impl Default for PluginNamespaceState {
    fn default() -> Self {
        Self {
            format_version: FormatVersion::ONE,
            generation: 0,
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
