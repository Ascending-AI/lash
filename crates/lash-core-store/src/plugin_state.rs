//! Durable plugin-namespace state carried in a session checkpoint.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Complete plugin-state checkpoint body, including non-resident namespaces.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginState {
    pub plugins: BTreeMap<String, PluginNamespaceState>,
}
/// One namespace's mediated generation and JSON values. Plugin versions are diagnostics only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginNamespaceState {
    pub generation: u64,
    pub values: BTreeMap<String, Value>,
}
