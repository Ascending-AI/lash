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
mod publication;
pub use lash_core_store::plugin_state::{KeyRejection, PluginNamespaceState, PluginState};
pub use lash_core_store::tool_run::{
    FrontierRefusal, HookCause, HookOccurrence, NamespaceFrontierRefusal, PublicationOrdinal,
    ResolvedStateChange, StateCommand, StateCommandOrigin, StateCommandRefusal, StateResolution,
    StateResolutionOutcome,
};
pub use publication::{EffectPublication, PluginStateEffect, StateReducer, StateReduction};
pub(crate) use publication::{Proposal, collect_proposals, propose, propose_all, record_effect};

use lash_core_store::plugin_state::{
    PLUGIN_STATE_NAMESPACE_LIMIT, PLUGIN_STATE_VALUE_LIMIT, validate_state_key,
};
use lash_core_store::tool_run::SegmentOrdinal;
use lash_sansio::sync::MutexExt;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

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
    #[error("recorded plugin state belongs to another runtime owner")]
    EffectOwnerMismatch,
    /// A recorded resolution does not follow its namespace's applied
    /// frontier: it skips a publication, or a predecessor publishes after
    /// ownership moved on.
    #[error("plugin `{plugin}` cannot apply a recorded publication: {refusal}")]
    Frontier {
        plugin: String,
        refusal: FrontierRefusal,
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

impl From<PluginStateError> for super::PluginError {
    fn from(error: PluginStateError) -> Self {
        Self::State(error)
    }
}

impl From<PluginStateError> for crate::RuntimeEffectControllerError {
    fn from(error: PluginStateError) -> Self {
        super::PluginError::State(error).into()
    }
}

impl PluginStateError {
    /// Every state failure is permanent for the same input: a codec or limit
    /// refusal does not change, and a fenced or out-of-order publication is
    /// resolved only by rebuilding from durable state.
    pub fn is_terminal(&self) -> bool {
        true
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
    owner: crate::RuntimeOwner,
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

    pub(super) fn bind(
        owner: &crate::RuntimeOwner,
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
            owner: owner.clone(),
            plugin_id: plugin_id.into(),
            state,
        }
    }

    pub fn owner(&self) -> &crate::RuntimeOwner {
        &self.owner
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// The namespace's published generation: the count of publications it
    /// has applied, which a checkpoint carries as its applied frontier.
    pub fn generation(&self) -> u64 {
        self.state
            .lock_recover()
            .data
            .plugins
            .get(self.plugin_id())
            .map_or(0, |namespace| namespace.generation)
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

pub(super) fn validate_namespace(values: &BTreeMap<String, Value>) -> Result<(), PluginStateError> {
    for (key, value) in values {
        validate_state_key(key).map_err(|reason| PluginStateError::InvalidKey {
            key: key.clone(),
            reason,
        })?;
        let bytes = serde_json::to_vec(value)
            .map_err(|error| PluginStateError::Encode {
                key: key.clone(),
                message: error.to_string(),
            })?
            .len();
        if bytes > PLUGIN_STATE_VALUE_LIMIT {
            return Err(PluginStateError::ValueTooLarge {
                key: key.clone(),
                bytes,
                limit: PLUGIN_STATE_VALUE_LIMIT,
            });
        }
    }
    let bytes = serde_json::to_vec(values)
        .map_err(|error| PluginStateError::Encode {
            key: String::new(),
            message: error.to_string(),
        })?
        .len();
    if bytes > PLUGIN_STATE_NAMESPACE_LIMIT {
        return Err(PluginStateError::StoreTooLarge {
            bytes,
            limit: PLUGIN_STATE_NAMESPACE_LIMIT,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// The resident published state of one owner, and its publication slots.
#[derive(Debug, Default)]
pub(super) struct PluginStateRegistry {
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
}

impl PluginStateRegistry {
    // Hydrate before admission so register calls validate durable membership and
    // size.
    pub(super) fn from_snapshot(snapshot: Option<&PluginState>) -> Self {
        Self {
            data: snapshot.cloned().unwrap_or_default(),
            ..Self::default()
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
