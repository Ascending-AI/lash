//! Host-mediated plugin state: read-only views, declared commands, and the
//! coordinator that publishes their recorded resolutions (K10, FIG-4878).
//!
//! No plugin holds a writable handle. A plugin reads its namespace through a
//! [`PluginStateView`], which shows only published state. A tool body, or a
//! before-turn, after-turn, checkpoint or after-tool callback, returns
//! [`StateCommands`] with its result. The recorded body that ran it reduces
//! them privately against the published namespace, records the resolution
//! with its result, and publishes it once the engine returns that result
//! durably. Replay installs the recorded resolution without running the
//! body, the callback or a reducer.
mod component;
mod publication;
mod run;
pub use component::ComponentState;
pub use lash_core_store::plugin_state::{
    KeyRejection, NamespaceBody, NamespaceEncoding, NamespaceEntry, NamespaceValues,
    PluginNamespaceState, PluginState, PluginStateError, StateFork,
};
pub use lash_core_store::tool_run::{
    FrontierRefusal, FrontierStep, HookCause, HookOccurrence, NamespaceFrontierRefusal,
    PublicationOrdinal, ResolvedStateChange, StateCommand, StateCommandOrigin, StateCommandRefusal,
    StateFrontier, StateResolution, StateResolutionOutcome,
};
pub use publication::{
    EffectPublication, PluginStateEffect, Proposal, StagedPluginState, StateReducer, StateReduction,
};
pub(crate) use publication::{collect_proposals, propose, propose_all, record_effect};

use lash_core_store::plugin_state::validate_state_key_with_limit;
use lash_core_store::tool_run::{SegmentOrdinal, StateCommandLimits};
use lash_sansio::sync::MutexExt;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

impl From<PluginStateError> for super::PluginError {
    fn from(error: PluginStateError) -> Self {
        Self::State(error)
    }
}

/// The commands a callback or tool body returns with its result: an ordered
/// batch against its own plugin's namespace. Later commands see earlier
/// ones; [`set`](Self::set) and [`remove`](Self::remove) are last-writer-wins,
/// and a read-modify-write goes through a registered pure reducer with
/// [`apply`](Self::apply).
///
/// The batch is bounded and all-or-none: one refused command publishes
/// nothing of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateCommands(Vec<StateCommand>);

impl StateCommands {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `key` to `value`.
    #[must_use]
    pub fn set(mut self, key: impl Into<String>, value: Value) -> Self {
        self.0.push(StateCommand::Set {
            key: key.into(),
            value,
        });
        self
    }

    /// Set `key` to `value`'s JSON encoding.
    ///
    /// # Errors
    ///
    /// [`PluginStateError::Encode`] when `value` does not encode.
    pub fn set_as<T: Serialize>(
        self,
        key: impl Into<String>,
        value: &T,
    ) -> Result<Self, PluginStateError> {
        let key = key.into();
        let value = serde_json::to_value(value).map_err(|source| PluginStateError::Encode {
            key: key.clone(),
            message: source.to_string(),
        })?;
        Ok(self.set(key, value))
    }

    /// Remove `key`.
    #[must_use]
    pub fn remove(mut self, key: impl Into<String>) -> Self {
        self.0.push(StateCommand::Remove { key: key.into() });
        self
    }

    /// Resolve `key` through the plugin's reducer `reducer` with `input`.
    #[must_use]
    pub fn apply(
        mut self,
        key: impl Into<String>,
        reducer: impl Into<String>,
        input: Value,
    ) -> Self {
        self.0.push(StateCommand::Apply {
            key: key.into(),
            name: reducer.into(),
            input,
        });
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn commands(&self) -> &[StateCommand] {
        &self.0
    }

    #[must_use]
    pub fn into_commands(self) -> Vec<StateCommand> {
        self.0
    }
}

impl From<Vec<StateCommand>> for StateCommands {
    fn from(commands: Vec<StateCommand>) -> Self {
        Self(commands)
    }
}

/// A plugin's read-only view of its namespace, for one session or process
/// owner. It shows published state only: a value appears once the recorded
/// resolution that wrote it is durable. Clones share the same view.
#[derive(Clone)]
pub struct PluginStateView {
    /// Who the plugin session this view belongs to was built for.
    owner: Option<crate::RuntimeOwner>,
    plugin_id: Arc<str>,
    state: Arc<Mutex<PluginStateRegistry>>,
}

impl std::fmt::Debug for PluginStateView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginStateView")
            .field("owner", &self.owner)
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PluginStateView {
    /// A handle whose strong count tells whether a plugin kept this view:
    /// every clone of the view shares it.
    pub(super) fn retention_probe(&self) -> Arc<str> {
        Arc::clone(&self.plugin_id)
    }

    pub(crate) fn bind(
        owner: &crate::RuntimeOwner,
        plugin_id: &str,
        state: Arc<Mutex<PluginStateRegistry>>,
    ) -> Self {
        Self::namespace(Some(owner.clone()), plugin_id, state)
    }

    pub(crate) fn detached(plugin_id: &str, state: Arc<Mutex<PluginStateRegistry>>) -> Self {
        Self::namespace(None, plugin_id, state)
    }

    fn namespace(
        owner: Option<crate::RuntimeOwner>,
        plugin_id: &str,
        state: Arc<Mutex<PluginStateRegistry>>,
    ) -> Self {
        state
            .lock_recover()
            .data
            .plugins
            .entry(plugin_id.to_owned())
            .or_default();
        Self {
            owner,
            plugin_id: plugin_id.into(),
            state,
        }
    }

    /// The materialized owner, absent when a component is bound for a deployment preview.
    pub fn owner(&self) -> Option<&crate::RuntimeOwner> {
        self.owner.as_ref()
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// The namespace's published generation: it advances with every
    /// publication the namespace applies and every format conversion of its
    /// values.
    pub fn generation(&self) -> u64 {
        self.state
            .lock_recover()
            .data
            .plugins
            .get(self.plugin_id())
            .map_or(0, |namespace| namespace.generation)
    }

    /// This namespace frozen at its published generation, read under one
    /// lock: a prompt renderer reading it never sees two generations.
    pub fn committed(&self) -> super::prompt::CommittedPluginNamespace {
        let registry = self.state.lock_recover();
        registry.data.plugins.get(self.plugin_id()).map_or_else(
            super::prompt::CommittedPluginNamespace::default,
            |namespace| {
                super::prompt::CommittedPluginNamespace::new(
                    namespace.generation,
                    namespace.values.clone(),
                )
            },
        )
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.state
            .lock_recover()
            .data
            .plugins
            .get(self.plugin_id())
            .and_then(|namespace| namespace.values.get(key))
            .cloned()
    }

    pub fn get_as<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, PluginStateError> {
        self.get(key)
            .map(|value| {
                serde_json::from_value(value).map_err(|source| PluginStateError::Decode {
                    key: key.into(),
                    message: source.to_string(),
                })
            })
            .transpose()
    }

    pub fn keys(&self) -> Vec<String> {
        self.state
            .lock_recover()
            .data
            .plugins
            .get(self.plugin_id())
            .map(|namespace| namespace.values.keys().cloned().collect())
            .unwrap_or_default()
    }
}

pub(super) fn validate_namespace(
    values: &BTreeMap<String, Value>,
    limits: StateCommandLimits,
) -> Result<(), PluginStateError> {
    for (key, value) in values {
        validate_state_key_with_limit(key, limits.key_bytes).map_err(|reason| {
            PluginStateError::InvalidKey {
                key: key.clone(),
                reason,
            }
        })?;
        let bytes = serde_json::to_vec(value)
            .map_err(|error| PluginStateError::Encode {
                key: key.clone(),
                message: error.to_string(),
            })?
            .len();
        if bytes > limits.max_value_bytes {
            return Err(PluginStateError::ValueTooLarge {
                key: key.clone(),
                bytes,
                limit: limits.max_value_bytes,
            });
        }
    }
    let bytes = serde_json::to_vec(values)
        .map_err(|error| PluginStateError::Encode {
            key: String::new(),
            message: error.to_string(),
        })?
        .len();
    if bytes > limits.max_namespace_bytes {
        return Err(PluginStateError::StoreTooLarge {
            bytes,
            limit: limits.max_namespace_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// The resident published state of one owner, and its publication slots.
#[derive(Debug, Default)]
pub(crate) struct PluginStateRegistry {
    pub(super) diagnostics: crate::DiagnosticLimits,
    pub(super) limits: lash_core_store::tool_run::StateCommandLimits,
    /// Published state: every namespace's values at its applied generation.
    pub(super) data: PluginState,
    pub(super) source: Option<crate::BlobRef>,
    /// The segment that owns publication; a resolution recorded by an
    /// earlier segment never applies (K6).
    segment: SegmentOrdinal,
    /// Namespaces with a reduced publication the engine has not yet
    /// returned, by the recorded effect that reduced it. The next reduction
    /// of such a namespace waits for it.
    reserved: BTreeMap<String, crate::EffectAddress>,
    /// Namespaces whose reduced publication was abandoned unreturned.
    fenced: BTreeSet<String>,
    /// Recorded resolutions a replay delivered ahead of a predecessor, by
    /// namespace and ordinal: each applies once its predecessor has.
    owed: BTreeMap<String, BTreeMap<u64, StateResolution>>,
    /// Woken whenever a reservation settles.
    settled: Arc<tokio::sync::Notify>,
    /// What a durable run's rows record, while the owner runs one.
    run: Option<run::RunRows>,
    /// Each namespace's encoded size, by the values it was measured from:
    /// what the session budget sums without encoding unchanged values again.
    sizes: BTreeMap<String, (std::sync::Weak<BTreeMap<String, Value>>, usize)>,
}

impl PluginStateRegistry {
    // Hydrate before admission so register calls validate durable membership and
    // size.
    pub(super) fn from_snapshot(snapshot: Option<&PluginState>) -> Self {
        let data = snapshot.cloned().unwrap_or_default();
        let segment = data
            .plugins
            .values()
            .map(|namespace| namespace.publication.owner_segment)
            .max()
            .unwrap_or_default();
        Self {
            data,
            segment,
            ..Self::default()
        }
    }

    pub(super) fn from_fork(snapshot: Option<&PluginState>) -> Self {
        let mut registry = Self::from_snapshot(snapshot);
        registry.segment = SegmentOrdinal::default();
        for namespace in registry.data.plugins.values_mut() {
            namespace.publication.owner_segment = registry.segment;
        }
        registry
    }

    /// Record `fork`, what `plugin` declares a fork does with its namespace,
    /// on the namespace: a fork follows the recorded policy.
    pub(super) fn declare_fork(
        &mut self,
        plugin: &str,
        fork: lash_core_store::plugin_state::StateFork,
    ) {
        let namespace = self.data.plugins.entry(plugin.to_owned()).or_default();
        if namespace.fork != fork {
            namespace.fork = fork;
            self.source = None;
        }
    }

    pub(super) fn matches_ref(&self, reference: &crate::BlobRef) -> bool {
        self.source.as_ref() == Some(reference) || state_ref(&self.data) == *reference
    }

    pub(super) fn was_hydrated_from(&self, snapshot: &PluginState) -> bool {
        self.source.as_ref() == Some(&state_ref(snapshot)) || self.data == *snapshot
    }

    /// Adopt a recorded head's state as the published state. Its namespace
    /// generations are the applied frontier the journal's recorded
    /// resolutions are delivered against; nothing unrecorded is resident to
    /// drop, and a rebuild from durable state lifts every fence.
    pub(super) fn hydrate_live(&mut self, snapshot: &PluginState) {
        if self.was_hydrated_from(snapshot) && self.fenced.is_empty() {
            return;
        }
        self.data = snapshot.clone();
        self.segment = self
            .data
            .plugins
            .values()
            .map(|namespace| namespace.publication.owner_segment)
            .max()
            .unwrap_or_default();
        self.reserved.clear();
        self.fenced.clear();
        self.owed.clear();
        self.source = Some(state_ref(snapshot));
        self.settled.notify_waiters();
    }
}

#[expect(
    clippy::expect_used,
    reason = "`PluginState` is a map of strings to `serde_json::Value`, which MessagePack encodes without a failing case"
)]
pub(super) fn state_ref(state: &PluginState) -> crate::BlobRef {
    crate::BlobRef::for_content(&rmp_serde::to_vec_named(state).expect("plugin state encodes"))
}
