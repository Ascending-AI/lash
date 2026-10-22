//! Durable protocol-owned execution state carried in a session checkpoint,
//! and a session's recorded plugin configuration.
use crate::plugin_state::{FormatVersion, PluginConfigNamespace};
use serde::de::DeserializeOwned;

use lash_protocol_types::allocation::{
    AllocationBudget, AllocationError, AllocationHold, SourceCapacity,
};
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
/// (ADR 0145 §9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionGraphPurpose {
    /// The complete post-cell state a turn phase owns as its
    /// `working_execution_ref` until the next successful cell or the final
    /// session transfer. Not a retained session revision.
    Working,
    /// The state a turn's final commit publishes with the session revision.
    Final,
}

/// A capture of a session's execution state (ADR 0145 §9): its session
/// descriptor page, which names the carried heap, binding and function
/// roots. Never a delta over an earlier root: the descriptor names the
/// complete state, and shares every unchanged heap page by reference. The
/// heap pages under it are the executor's to publish, with the commit that
/// names the root (FIG-6418); a capture carries none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExecutionGraphCapture {
    /// The session holds no execution state.
    #[default]
    Clear,
    /// The session's descriptor.
    Replace(HydratedExecutionGraph),
}

/// A session's execution state as a host holds it: its descriptor page's
/// id and bytes. A hydrate never descends into heap pages (ADR 0145 §10);
/// a binding's pages are read on demand under a root lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedExecutionGraph {
    pub root: crate::checkpoint_graph::NodeId,
    pub descriptor: lash_kernel_doc::allocation::SharedBytes,
}

impl HydratedExecutionGraph {
    /// The descriptor as the node a commit stages.
    #[must_use]
    pub fn node(&self) -> crate::checkpoint_graph::EncodedNode {
        crate::checkpoint_graph::EncodedNode {
            id: self.root,
            bytes: self.descriptor.clone(),
        }
    }
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
#[serde(transparent)]
pub struct PluginConfig(Arc<PluginConfigStorage>);

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PluginConfigStorage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    namespaces: BTreeMap<String, PluginConfigNamespace>,
    #[serde(skip)]
    allocation: AllocationHold,
    #[serde(skip)]
    budget: AllocationBudget,
}
impl PartialEq for PluginConfigStorage {
    fn eq(&self, other: &Self) -> bool {
        self.protocol == other.protocol && self.namespaces == other.namespaces
    }
}
impl Eq for PluginConfigStorage {}
impl PluginConfig {
    /// A configuration recorded under the protocol plugin `protocol`.
    pub fn for_protocol(protocol: Option<String>) -> Self {
        Self::from_recorded_parts(protocol, BTreeMap::new())
    }

    pub fn is_empty(&self) -> bool {
        self.0.protocol.is_none() && self.0.namespaces.is_empty()
    }

    /// A configuration a transport decoded: the protocol owner and the
    /// namespaces exactly as recorded where it was encoded.
    pub fn from_recorded_parts(
        protocol: Option<String>,
        namespaces: BTreeMap<String, PluginConfigNamespace>,
    ) -> Self {
        Self(Arc::new(PluginConfigStorage {
            protocol,
            namespaces,
            ..Default::default()
        }))
    }

    /// The protocol owner and the namespaces, for a transport to encode.
    pub fn recorded_parts(&self) -> (Option<&str>, &BTreeMap<String, PluginConfigNamespace>) {
        (self.protocol_plugin_id(), self.namespaces())
    }

    /// The ledger that admitted this immutable configuration.
    pub fn allocation_budget(&self) -> &AllocationBudget {
        &self.0.budget
    }

    /// Share an admitted source on this ledger, or reserve before copying
    /// the map, identifiers and foreign namespace bodies.
    pub fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        self.copy_in(budget)
    }

    fn map_capacity(&self) -> u64 {
        self.0.namespaces.keys().fold(
            self.0
                .protocol
                .as_ref()
                .map_or(0, |id| id.owned_capacity())
                .saturating_add(std::mem::size_of::<PluginConfigStorage>() as u64 + 64),
            |bytes, key| {
                bytes
                    .saturating_add(key.owned_capacity())
                    .saturating_add(2048)
            },
        )
    }

    fn copy_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        let allocation = budget.reserve(self.map_capacity())?;
        let namespaces = self
            .0
            .namespaces
            .iter()
            .map(|(id, namespace)| {
                Ok((
                    id.clone(),
                    PluginConfigNamespace {
                        format_version: namespace.format_version,
                        value: namespace.value.clone_in(budget)?,
                    },
                ))
            })
            .collect::<Result<_, AllocationError>>()?;
        Ok(Self(Arc::new(PluginConfigStorage {
            protocol: self.0.protocol.clone(),
            namespaces,
            allocation,
            budget: budget.clone(),
        })))
    }

    /// A caller that reserved its entire source before decoding transfers
    /// that same obligation to the fresh map and every escaping JSON reader.
    pub fn decoded_in(
        &mut self,
        allocation: AllocationHold,
        budget: AllocationBudget,
    ) -> Result<(), crate::StoreError> {
        let storage =
            Arc::get_mut(&mut self.0).ok_or_else(|| crate::StoreError::StoredDataCorrupt {
                record_kind: "PluginConfig",
                message: "a fresh decode unexpectedly shares config before admission".into(),
            })?;
        for namespace in storage.namespaces.values_mut() {
            namespace
                .value
                .decoded_in(allocation.clone(), budget.clone())?;
        }
        storage.allocation = allocation;
        storage.budget = budget;
        Ok(())
    }

    /// The session's protocol plugin id, if it recorded one.
    pub fn protocol_plugin_id(&self) -> Option<&str> {
        self.0.protocol.as_deref()
    }

    /// The namespace `plugin_id` recorded, if any.
    pub fn get(&self, plugin_id: &str) -> Option<&serde_json::Value> {
        self.0
            .namespaces
            .get(plugin_id)
            .map(|namespace| namespace.value.as_ref())
    }

    pub fn namespace(&self, plugin_id: &str) -> Option<&PluginConfigNamespace> {
        self.0.namespaces.get(plugin_id)
    }

    pub fn namespaces(&self) -> &BTreeMap<String, PluginConfigNamespace> {
        &self.0.namespaces
    }

    pub fn insert_versioned(
        &mut self,
        plugin_id: impl AsRef<str>,
        format_version: FormatVersion,
        value: serde_json::Value,
    ) -> Result<(), AllocationError> {
        let plugin_id = plugin_id.as_ref();
        let budget = &self.0.budget;
        // Admit the replacement map before cloning any owned identifiers or
        // JSON. Publish only after every reservation succeeds.
        let allocation = budget.reserve(
            self.map_capacity()
                .saturating_add(plugin_id.len() as u64 + 32)
                .saturating_add(2048),
        )?;
        let value = crate::plugin_state::ConfigJson::new_in(value, budget)?;
        let mut namespaces = self.0.namespaces.clone();
        namespaces.insert(
            plugin_id.to_owned(),
            PluginConfigNamespace {
                format_version,
                value,
            },
        );
        *self = Self(Arc::new(PluginConfigStorage {
            protocol: self.0.protocol.clone(),
            namespaces,
            allocation,
            budget: budget.clone(),
        }));
        Ok(())
    }

    /// Decode the namespace `plugin_id` recorded, if any.
    pub fn decode<T>(&self, plugin_id: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        self.get(plugin_id)
            .map(serde::Deserialize::deserialize)
            .transpose()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &serde_json::Value)> {
        self.0
            .namespaces
            .iter()
            .map(|(id, namespace)| (id, namespace.value.as_ref()))
    }

    /// The session's protocol turn options: a view of the protocol plugin's
    /// namespace, empty when it recorded none.
    pub fn protocol_turn_options(&self) -> crate::ProtocolTurnOptions {
        self.0
            .protocol
            .as_deref()
            .and_then(|protocol| self.namespace(protocol))
            .map(|namespace| crate::ProtocolTurnOptions::from_payload(namespace.value.clone()))
            .unwrap_or_default()
    }

    /// Record `value` as `plugin_id`'s namespace, replacing what it held.
    /// Only an owner's output reaches this: what it created, or its recorded
    /// namespace with a run's options applied.
    pub fn insert(
        &mut self,
        plugin_id: impl AsRef<str>,
        value: serde_json::Value,
    ) -> Result<(), AllocationError> {
        let plugin_id = plugin_id.as_ref();
        let format_version = self
            .0
            .namespaces
            .get(plugin_id)
            .map_or(FormatVersion::ONE, |ns| ns.format_version);
        self.insert_versioned(plugin_id, format_version, value)
    }

    /// Replace each namespace `updates` names with its value.
    pub fn apply_namespace_updates(
        &mut self,
        updates: &BTreeMap<String, PluginConfigNamespace>,
    ) -> Result<(), AllocationError> {
        if updates.is_empty() {
            return Ok(());
        }
        let budget = &self.0.budget;
        let extra = updates.keys().fold(0u64, |bytes, key| {
            bytes
                .saturating_add(key.owned_capacity())
                .saturating_add(2048)
        });
        let allocation = budget.reserve(self.map_capacity().saturating_add(extra))?;
        // The temporary map is admitted too; no partially applied edit escapes.
        let _scratch = budget.reserve(extra)?;
        let updates = updates
            .iter()
            .map(|(id, namespace)| {
                Ok((
                    id.clone(),
                    PluginConfigNamespace {
                        format_version: namespace.format_version,
                        value: namespace.value.clone_in(budget)?,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>, AllocationError>>()?;
        let mut namespaces = self.0.namespaces.clone();
        namespaces.extend(updates);
        *self = Self(Arc::new(PluginConfigStorage {
            protocol: self.0.protocol.clone(),
            namespaces,
            allocation,
            budget: budget.clone(),
        }));
        Ok(())
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
                value: value.into(),
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
                    match (base.value.as_ref().clone(), top.value.as_ref().clone()) {
                        (serde_json::Value::Object(mut fields), serde_json::Value::Object(top)) => {
                            fields.extend(top);
                            base.value = serde_json::Value::Object(fields).into();
                        }
                        (_, top) => base.value = top.into(),
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
            .map(|namespace| serde::Deserialize::deserialize(namespace.value.as_ref()))
            .transpose()
    }
}

impl crate::store::DurableRecord for PluginOptions {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::session_graph::SESSION_NODE_BODY_SCHEMA_VERSION);
}

#[cfg(test)]
pub(crate) mod retention_tests {
    use super::*;
    use lash_protocol_types::allocation::{
        AllocationBudget, AllocationError, AllocationHold, AllocationLedger, AllocationReservation,
        ReserveError,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone)]
    pub(crate) struct Ledger {
        pub(crate) used: Arc<AtomicU64>,
        pub(crate) limit: u64,
    }
    struct Hold {
        ledger: Ledger,
        bytes: u64,
    }
    impl AllocationReservation for Hold {
        fn retain(&self) {}
    }
    impl Drop for Hold {
        fn drop(&mut self) {
            self.ledger.used.fetch_sub(self.bytes, Ordering::SeqCst);
        }
    }
    impl AllocationLedger for Ledger {
        fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
            self.used
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                    used.checked_add(bytes).filter(|total| *total <= self.limit)
                })
                .map_err(|_| AllocationError {
                    kind: ReserveError::Pressure,
                    actor: "config".into(),
                    requested: bytes,
                    capacity: self.limit,
                })?;
            Ok(AllocationHold::new(Hold {
                ledger: self.clone(),
                bytes,
            }))
        }
        fn reserve_shared(&self, _: usize, bytes: u64) -> Result<AllocationHold, AllocationError> {
            self.reserve(bytes)
        }
    }

    /// LO-EVICT-CONFIG: a namespace and a protocol view pin their frame's
    /// source admission until the last escaped immutable reader drops.
    #[test]
    fn frame_config_admission_follows_escaped_namespace_and_protocol_readers() {
        let mut config = PluginConfig::for_protocol(Some("protocol".into()));
        config
            .insert("protocol", serde_json::json!({"text": "x".repeat(4096)}))
            .unwrap();
        let frame = crate::SessionNodeRecord::new(
            crate::NodeId::fixture("frame"),
            None,
            "2026-10-22T00:00:00.000000000Z".parse().unwrap(),
            crate::SessionNodePayload::FrameOpen {
                frame_key: crate::FrameKey::from_caller_material("config").unwrap(),
                reason: crate::AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::new(
                    crate::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(16),
                        crate::NoProgressBudget::bounded(3),
                    ),
                    config,
                ),
            },
        );
        let fleet = crate::store::FleetFormat::current();
        let bytes = frame.encode_storage_body(fleet).unwrap();
        let used = Arc::new(AtomicU64::new(0));
        let budget = AllocationBudget::new(Ledger {
            used: used.clone(),
            limit: 1 << 22,
        });
        let decoded =
            crate::SessionNodeRecord::decode_storage_body_in("frame", None, &bytes, fleet, &budget)
                .unwrap();
        assert_eq!(decoded.encode_storage_body(fleet).unwrap(), bytes);
        let crate::SessionNodePayload::FrameOpen { assignment, .. } = &decoded.payload else {
            unreachable!()
        };
        let namespace = assignment
            .plugin_config
            .namespace("protocol")
            .unwrap()
            .value
            .clone();
        let protocol = assignment.plugin_config.protocol_turn_options().payload;
        let charged = used.load(Ordering::SeqCst);
        assert!(charged >= bytes.len() as u64);
        drop(decoded);
        assert_eq!(
            used.load(Ordering::SeqCst),
            charged,
            "escaped config readers must retain their admission"
        );
        drop(namespace);
        assert_eq!(used.load(Ordering::SeqCst), charged);
        assert_eq!(protocol.as_ref()["text"], "x".repeat(4096));
        drop(protocol);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }

    /// LO-EVICT-CONFIG-CLONE: immutable config clones share their map and
    /// identifier allocations rather than allocating outside admission.
    #[test]
    fn config_clone_shares_namespace_map_and_identifiers() {
        let mut config = PluginConfig::for_protocol(Some("protocol".repeat(100)));
        for index in 0..64 {
            config
                .insert(format!("plugin-{index}"), serde_json::Value::Null)
                .unwrap();
        }
        let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
        let copy = config.clone();
        let counts = measurement.finish();
        assert_eq!(config, copy);
        assert_eq!(
            counts.allocated, 0,
            "a config clone must share the admitted immutable source"
        );
        let used = Arc::new(AtomicU64::new(0));
        let tiny = AllocationBudget::new(Ledger {
            used: used.clone(),
            limit: 1,
        });
        let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
        assert!(config.clone_in(&tiny).is_err());
        let refused = measurement.finish();
        assert!(
            refused.allocated < 64,
            "refusal must precede map/string allocation: {refused:?}"
        );
        assert_eq!(used.load(Ordering::SeqCst), 0);
        let budget = AllocationBudget::new(Ledger {
            used: used.clone(),
            limit: 1 << 22,
        });
        let mut admitted = config.clone_in(&budget).unwrap();
        let charged = used.load(Ordering::SeqCst);
        assert!(charged >= config.map_capacity());
        let clone = admitted.clone();
        let blocker = budget.reserve((1 << 22) - charged).unwrap();
        assert!(admitted.insert("new", serde_json::Value::Null).is_err());
        assert_eq!(admitted, clone, "a refused replacement publishes nothing");
        assert_eq!(used.load(Ordering::SeqCst), 1 << 22);
        drop(blocker);
        admitted.insert("new", serde_json::Value::Null).unwrap();
        assert!(clone.namespace("new").is_none());
        assert!(used.load(Ordering::SeqCst) > charged);
        drop(admitted);
        assert_eq!(used.load(Ordering::SeqCst), charged);
        drop(clone);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }
}
