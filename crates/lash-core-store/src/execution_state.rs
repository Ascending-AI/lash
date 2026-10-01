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
/// A session's recorded plugin configuration (FIG-4379): each installed
/// owner's canonical namespace, keyed by plugin id, resolved defaults
/// included, and which of them is the session's protocol plugin.
///
/// It is what the owners made of a request, never the request itself: a
/// creation's or a patch's [`PluginOptions`] reach it only through their
/// owner's validation. It is recorded with the session's config head, moves
/// only with the config's revision, and every open delivers it unchanged.
/// The protocol plugin's namespace is recorded here like every other owner's;
/// the protocol turn options every turn reads are a view of it
/// ([`Self::protocol_turn_options`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    /// The session's protocol plugin: the owner whose namespace is the
    /// session's protocol turn options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    namespaces: BTreeMap<String, serde_json::Value>,
}
impl PluginConfig {
    /// A configuration recorded under the protocol plugin `protocol`.
    pub fn for_protocol(protocol: Option<String>) -> Self {
        Self {
            protocol,
            namespaces: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.protocol.is_none() && self.namespaces.is_empty()
    }

    /// A configuration a transport decoded: the protocol owner and the
    /// namespaces exactly as recorded where it was encoded.
    pub fn from_recorded_parts(
        protocol: Option<String>,
        namespaces: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            protocol,
            namespaces,
        }
    }

    /// The protocol owner and the namespaces, for a transport to encode.
    pub fn into_recorded_parts(self) -> (Option<String>, BTreeMap<String, serde_json::Value>) {
        (self.protocol, self.namespaces)
    }

    /// The session's protocol plugin id, if it recorded one.
    pub fn protocol_plugin_id(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    /// The namespace `plugin_id` recorded, if any.
    pub fn get(&self, plugin_id: &str) -> Option<&serde_json::Value> {
        self.namespaces.get(plugin_id)
    }

    /// Decode the namespace `plugin_id` recorded, if any.
    pub fn decode<T>(&self, plugin_id: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        self.namespaces
            .get(plugin_id)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &serde_json::Value)> {
        self.namespaces.iter()
    }

    /// The session's protocol turn options: a view of the protocol plugin's
    /// namespace, empty when it recorded none.
    pub fn protocol_turn_options(&self) -> crate::ProtocolTurnOptions {
        self.protocol
            .as_deref()
            .and_then(|protocol| self.namespaces.get(protocol))
            .cloned()
            .map(crate::ProtocolTurnOptions::from_payload)
            .unwrap_or_default()
    }

    /// Record `value` as `plugin_id`'s namespace, replacing what it held.
    /// Only an owner's output reaches this: what it created, or its recorded
    /// namespace with a run's options applied.
    pub fn insert(&mut self, plugin_id: impl Into<String>, value: serde_json::Value) {
        self.namespaces.insert(plugin_id.into(), value);
    }

    /// Replace each namespace `updates` names with its value.
    pub fn apply_namespace_updates(&mut self, updates: &BTreeMap<String, serde_json::Value>) {
        for (plugin_id, value) in updates {
            self.namespaces.insert(plugin_id.clone(), value.clone());
        }
    }
}

/// A plugin configuration at the config revision it was recorded under
/// (FIG-4379): what a root was admitted under (its recorded `ResolvedRun`),
/// what a process captured with its execution environment, or the head's
/// outside any root. Hooks read their configuration from this, never from
/// the session's current head.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedPluginConfig {
    /// The session config revision this configuration was recorded under.
    pub revision: u64,
    pub config: Arc<PluginConfig>,
}
impl AdmittedPluginConfig {
    pub fn new(config: PluginConfig, revision: u64) -> Self {
        Self {
            revision,
            config: Arc::new(config),
        }
    }

    /// Decode the namespace `plugin_id` recorded, if any.
    pub fn decode<T>(&self, plugin_id: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        self.config.decode(plugin_id)
    }
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

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// `self` over `under`, plugin by plugin. Where both state a JSON object
    /// for one plugin, its fields merge and `self`'s win; any other value of
    /// `self` replaces `under`'s. So a session's spec can state one option of
    /// a plugin and keep the rest of the defaults beneath it.
    #[must_use]
    pub fn over(self, mut under: Self) -> Self {
        for (plugin_id, top) in self.plugins {
            match (under.plugins.get_mut(&plugin_id), top) {
                (Some(serde_json::Value::Object(base)), serde_json::Value::Object(top)) => {
                    base.extend(top);
                }
                (_, top) => {
                    under.plugins.insert(plugin_id, top);
                }
            }
        }
        under
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
