//! Durable protocol-owned execution state carried in a session checkpoint.
use serde::de::DeserializeOwned;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Complete protocol-owned execution-state component update for one checkpoint.
///
/// `root` is the well-known execution-state root body. `components` is the
/// complete leaf-key listing reachable from that root: a changed body submits
/// new logical bytes, while an unchanged key reuses its resident durable ref.
/// An absent key is deleted. An absent root requires an empty leaf set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionStateSnapshot {
    pub root: Option<Arc<[u8]>>,
    pub components: BTreeMap<String, ExecutionStateComponentSnapshot>,
}
impl ExecutionStateSnapshot {
    pub fn from_root(root: Option<Arc<[u8]>>) -> Self {
        Self {
            root,
            components: BTreeMap::new(),
        }
    }

    pub fn changed_component(&mut self, key: impl Into<String>, body: impl Into<Arc<[u8]>>) {
        self.components.insert(
            key.into(),
            ExecutionStateComponentSnapshot::Changed(body.into()),
        );
    }

    pub fn unchanged_component(&mut self, key: impl Into<String>) {
        self.components
            .insert(key.into(), ExecutionStateComponentSnapshot::Unchanged);
    }

    pub fn from_hydrated(state: HydratedExecutionState) -> Self {
        Self {
            root: Some(state.root),
            components: state
                .components
                .into_iter()
                .map(|(key, body)| (key, ExecutionStateComponentSnapshot::Changed(body)))
                .collect(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionStateComponentSnapshot {
    Changed(Arc<[u8]>),
    Unchanged,
}
/// Fully hydrated protocol-owned execution state supplied during restore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HydratedExecutionState {
    pub root: Arc<[u8]>,
    pub components: BTreeMap<String, Arc<[u8]>>,
}
/// Plugin-owned options carried on a `SessionCreateRequest`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginOptions {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, serde_json::Value>,
}
impl PluginOptions {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Serializes one plugin's typed options for protocol implementors assembling session input.
    pub fn typed<T>(plugin_id: impl Into<String>, extras: T) -> Result<Self, serde_json::Error>
    where
        T: Serialize,
    {
        let mut options = Self::default();
        options.insert_typed(plugin_id, extras)?;
        Ok(options)
    }

    pub fn insert_typed<T>(
        &mut self,
        plugin_id: impl Into<String>,
        extras: T,
    ) -> Result<(), serde_json::Error>
    where
        T: Serialize,
    {
        self.plugins
            .insert(plugin_id.into(), serde_json::to_value(extras)?);
        Ok(())
    }

    pub fn decode<T>(&self, plugin_id: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        self.plugins
            .get(plugin_id)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
    }
}
