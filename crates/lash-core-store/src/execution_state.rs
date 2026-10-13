//! Durable protocol-owned execution state carried in a session checkpoint,
//! and a session's recorded plugin configuration.
use crate::plugin_state::{FormatVersion, PluginConfigNamespace};
use serde::de::DeserializeOwned;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Interpretation of a stored checkpoint key. Manifests retain their string keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointComponentKey {
    /// The retired tool-state component: refused at decode (FIG-6074).
    RetiredToolState,
    PluginState,
    /// One plugin namespace's values body, by plugin.
    PluginNamespace(String),
    Other(String),
}
impl CheckpointComponentKey {
    pub fn parse(key: &str) -> Self {
        match key {
            crate::store::RETIRED_TOOL_STATE_CHECKPOINT_COMPONENT => Self::RetiredToolState,
            crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT => Self::PluginState,
            _ => match crate::plugin_state::namespace_component_plugin(key) {
                Some(plugin) => Self::PluginNamespace(plugin.to_owned()),
                None => Self::Other(key.to_owned()),
            },
        }
    }
}

/// Which capture of a session's execution state a graph capture is
/// (FIG-5887 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionGraphPurpose {
    /// The complete post-cell state a turn phase owns as its
    /// `working_execution_ref` until the next successful cell or the final
    /// session transfer. Not a retained session revision.
    Working,
    /// The state a turn's final commit publishes with the session revision.
    Final,
}

/// A capture of a session's execution state (FIG-5887 §4, §6): a complete
/// SessionExecution root. Never a delta over an earlier root: the
/// root owns every adopted binding and function, sharing immutable leaves
/// and function bundles by reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExecutionGraphCapture {
    /// The session holds no execution state.
    #[default]
    Clear,
    Replace {
        /// A SessionExecution node.
        root: crate::checkpoint_graph::NodeId,
        /// Nodes the root needs that no acknowledged root is known to have
        /// installed. A commit that finds a reference missing refuses, and
        /// the executor resends the node's retained bytes; staged nodes are
        /// kept until the capture is acknowledged.
        nodes: BTreeMap<crate::checkpoint_graph::NodeId, crate::checkpoint_graph::EncodedNode>,
    },
}

/// A session's execution state as a hydrated graph: the SessionExecution root
/// and every node reachable from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedExecutionGraph {
    pub root: crate::checkpoint_graph::NodeId,
    pub nodes: BTreeMap<crate::checkpoint_graph::NodeId, crate::checkpoint_graph::EncodedNode>,
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
    namespaces: BTreeMap<String, PluginConfigNamespace>,
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
        namespaces: BTreeMap<String, PluginConfigNamespace>,
    ) -> Self {
        Self {
            protocol,
            namespaces,
        }
    }

    /// The protocol owner and the namespaces, for a transport to encode.
    pub fn into_recorded_parts(self) -> (Option<String>, BTreeMap<String, PluginConfigNamespace>) {
        (self.protocol, self.namespaces)
    }

    /// The session's protocol plugin id, if it recorded one.
    pub fn protocol_plugin_id(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    /// The namespace `plugin_id` recorded, if any.
    pub fn get(&self, plugin_id: &str) -> Option<&serde_json::Value> {
        self.namespaces
            .get(plugin_id)
            .map(|namespace| &namespace.value)
    }

    pub fn namespace(&self, plugin_id: &str) -> Option<&PluginConfigNamespace> {
        self.namespaces.get(plugin_id)
    }

    pub fn namespaces(&self) -> &BTreeMap<String, PluginConfigNamespace> {
        &self.namespaces
    }

    pub fn insert_versioned(
        &mut self,
        plugin_id: impl Into<String>,
        format_version: FormatVersion,
        value: serde_json::Value,
    ) {
        self.namespaces.insert(
            plugin_id.into(),
            PluginConfigNamespace {
                format_version,
                value,
            },
        );
    }

    /// Decode the namespace `plugin_id` recorded, if any.
    pub fn decode<T>(&self, plugin_id: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        self.get(plugin_id)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &serde_json::Value)> {
        self.namespaces
            .iter()
            .map(|(id, namespace)| (id, &namespace.value))
    }

    /// The session's protocol turn options: a view of the protocol plugin's
    /// namespace, empty when it recorded none.
    pub fn protocol_turn_options(&self) -> crate::ProtocolTurnOptions {
        self.protocol
            .as_deref()
            .and_then(|protocol| self.get(protocol))
            .cloned()
            .map(crate::ProtocolTurnOptions::from_payload)
            .unwrap_or_default()
    }

    /// Record `value` as `plugin_id`'s namespace, replacing what it held.
    /// Only an owner's output reaches this: what it created, or its recorded
    /// namespace with a run's options applied.
    pub fn insert(&mut self, plugin_id: impl Into<String>, value: serde_json::Value) {
        let plugin_id = plugin_id.into();
        let format_version = self
            .namespaces
            .get(&plugin_id)
            .map_or(FormatVersion::ONE, |ns| ns.format_version);
        self.insert_versioned(plugin_id, format_version, value);
    }

    /// Replace each namespace `updates` names with its value.
    pub fn apply_namespace_updates(&mut self, updates: &BTreeMap<String, PluginConfigNamespace>) {
        for (plugin_id, value) in updates {
            self.namespaces.insert(plugin_id.clone(), value.clone());
        }
    }
}

/// A plugin configuration at the config revision it was recorded under
/// (FIG-4379): what a run was admitted under (its recorded `ResolvedRun`),
/// what a process captured with its execution environment, or the head's
/// outside any run. Hooks read their configuration from this, never from
/// the session's current head.
///
/// The revision names the session config the configuration derives from;
/// the spec hash names what a run stated over it. Two runs at one revision
/// execute the same configuration exactly when their spec hashes agree
/// (FIG-6021).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedPluginConfig {
    /// The session config revision this configuration was recorded under.
    pub revision: u64,
    /// The run spec whose options derived this configuration from that
    /// revision's; `None` for the session's own configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<crate::run_spec::RunSpecHash>,
    pub config: Arc<PluginConfig>,
}
impl AdmittedPluginConfig {
    pub fn new(config: PluginConfig, revision: u64) -> Self {
        Self {
            revision,
            spec: None,
            config: Arc::new(config),
        }
    }

    /// This configuration as run spec `spec` derived it.
    #[must_use]
    pub fn derived_by(mut self, spec: Option<crate::run_spec::RunSpecHash>) -> Self {
        self.spec = spec;
        self
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
    pub plugins: BTreeMap<String, PluginConfigNamespace>,
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
        self.insert_versioned(plugin_id, FormatVersion::ONE, serde_json::to_value(extras)?);
        Ok(())
    }

    pub fn insert_versioned(
        &mut self,
        plugin_id: impl Into<String>,
        format_version: FormatVersion,
        value: serde_json::Value,
    ) {
        self.plugins.insert(
            plugin_id.into(),
            PluginConfigNamespace {
                format_version,
                value,
            },
        );
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
            match under.plugins.get_mut(&plugin_id) {
                Some(base) if base.format_version == top.format_version => {
                    match (&mut base.value, top.value) {
                        (serde_json::Value::Object(base), serde_json::Value::Object(top)) => {
                            base.extend(top)
                        }
                        (base, top) => *base = top,
                    }
                }
                _ => {
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
            .map(|namespace| namespace.value.clone())
            .map(serde_json::from_value)
            .transpose()
    }
}

impl crate::store::DurableRecord for PluginOptions {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::session_graph::SESSION_NODE_BODY_SCHEMA_VERSION);
}
