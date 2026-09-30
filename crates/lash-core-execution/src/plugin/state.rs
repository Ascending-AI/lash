//! Host-mediated plugin state and its deterministic checkpoint representation.
pub use lash_core_store::plugin_state::{PluginNamespaceState, PluginState};

use crate::SessionId;
use lash_sansio::sync::MutexExt;
use serde::Serialize;
use serde_json::Value;
#[allow(unused_imports)]
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const VALUE_LIMIT: usize = 32 * 1024;
const STORE_LIMIT: usize = 128 * 1024;

/// A deterministic rejection of a plugin-state key.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum KeyRejection {
    #[error("empty key")]
    Empty,
    #[error("key exceeds 128 bytes")]
    TooLong,
    #[error("illegal byte {byte} at {at}")]
    IllegalCharacter { at: usize, byte: u8 },
}

/// Rejections are atomic: neither values nor generation change.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginStateError {
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
    #[error("generation conflict: expected {expected}, actual {actual}")]
    GenerationConflict { expected: u64, actual: u64 },
}

impl From<PluginStateError> for super::PluginError {
    fn from(error: PluginStateError) -> Self {
        Self::State(error)
    }
}

impl PluginStateError {
    /// Validation and codec refusals are permanent for the same input.
    /// A generation conflict requires re-reading state and choosing a new edit.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::GenerationConflict { .. })
    }
}

/// A single edit in an atomic batch.
#[derive(Clone, Debug)]
pub enum PluginStateEdit {
    Set { key: String, value: Value },
    Remove { key: String },
}

/// Opaque capability for one session and plugin. Clones share read-your-writes;
/// writes become durable only at the next runtime boundary commit.
#[derive(Clone)]
pub struct PluginStateStore {
    /// Who the plugin session this store belongs to was built for.
    owner: crate::RuntimeOwner,
    plugin_id: Arc<str>,
    state: Arc<Mutex<PluginStateRegistry>>,
}

impl std::fmt::Debug for PluginStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginStateStore")
            .field("owner", &self.owner)
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PluginStateStore {
    /// A handle whose strong count tells whether a plugin kept this store:
    /// every clone of the store shares it.
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
    pub fn generation(&self) -> u64 {
        self.state.lock_recover().data.plugins[self.plugin_id()].generation
    }
    pub fn get(&self, key: &str) -> Option<Value> {
        self.state.lock_recover().data.plugins[self.plugin_id()]
            .values
            .get(key)
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
        self.state.lock_recover().data.plugins[self.plugin_id()]
            .values
            .keys()
            .cloned()
            .collect()
    }
    pub fn set(&self, key: &str, value: Value) -> Result<u64, PluginStateError> {
        self.apply(vec![PluginStateEdit::Set {
            key: key.into(),
            value,
        }])
    }
    pub fn set_as<T: Serialize>(&self, key: &str, value: &T) -> Result<u64, PluginStateError> {
        validate_key(key)?;
        let value = serde_json::to_value(value).map_err(|source| PluginStateError::Encode {
            key: key.into(),
            message: source.to_string(),
        })?;
        self.set(key, value)
    }
    #[expect(
        clippy::expect_used,
        reason = "the store is constructed bound to a plugin id whose namespace the runtime inserts at bind time, \
                  and a u64 generation counter cannot be exhausted"
    )]
    pub fn remove(&self, key: &str) -> Result<u64, PluginStateError> {
        validate_key(key)?;
        let mut state = self.state.lock_recover();
        let namespace = state
            .data
            .plugins
            .get_mut(self.plugin_id())
            .expect("bound namespace");
        let removed = namespace.values.contains_key(key);
        if removed {
            let generation = namespace
                .generation
                .checked_add(1)
                .expect("plugin generation exhausted");
            namespace.values.remove(key);
            namespace.generation = generation;
        }
        let generation = namespace.generation;
        if removed {
            state.record(
                self.plugin_id(),
                vec![PluginStateEdit::Remove { key: key.into() }],
            );
        }
        Ok(generation)
    }
    pub fn apply(&self, edits: Vec<PluginStateEdit>) -> Result<u64, PluginStateError> {
        self.edit(None, edits)
    }
    pub fn apply_guarded(
        &self,
        expected_generation: u64,
        edits: Vec<PluginStateEdit>,
    ) -> Result<u64, PluginStateError> {
        self.edit(Some(expected_generation), edits)
    }
    #[expect(
        clippy::expect_used,
        reason = "the store is constructed bound to a plugin id whose namespace the runtime inserts at bind time, \
                  a `serde_json::Value` re-encodes without a failing case, and a u64 generation counter \
                  cannot be exhausted"
    )]
    fn edit(
        &self,
        expected: Option<u64>,
        edits: Vec<PluginStateEdit>,
    ) -> Result<u64, PluginStateError> {
        let mut state = self.state.lock_recover();
        let namespace = state
            .data
            .plugins
            .get_mut(self.plugin_id())
            .expect("bound namespace");
        if let Some(expected) = expected
            && expected != namespace.generation
        {
            return Err(PluginStateError::GenerationConflict {
                expected,
                actual: namespace.generation,
            });
        }
        let registered_edits = if matches!(state.phase, StatePhase::Registering(_)) {
            Some(edits.clone())
        } else {
            None
        };
        let namespace = state
            .data
            .plugins
            .get_mut(self.plugin_id())
            .expect("bound namespace");
        let mut values = namespace.values.clone();
        for edit in edits {
            match edit {
                PluginStateEdit::Set { key, mut value } => {
                    validate_key(&key)?;
                    let bytes = serde_json::to_vec(&value)
                        .expect("JSON value encodes")
                        .len();
                    if bytes > VALUE_LIMIT {
                        return Err(PluginStateError::ValueTooLarge {
                            key,
                            bytes,
                            limit: VALUE_LIMIT,
                        });
                    }
                    value.sort_all_objects();
                    values.insert(key, value);
                }
                PluginStateEdit::Remove { key } => {
                    validate_key(&key)?;
                    values.remove(&key);
                }
            }
        }
        let bytes = serde_json::to_vec(&values).expect("JSON map encodes").len();
        if bytes > STORE_LIMIT {
            return Err(PluginStateError::StoreTooLarge {
                bytes,
                limit: STORE_LIMIT,
            });
        }
        let generation = namespace
            .generation
            .checked_add(1)
            .expect("plugin generation exhausted");
        namespace.values = values;
        namespace.generation = generation;
        if let Some(edits) = registered_edits {
            state.record(self.plugin_id(), edits);
        }
        Ok(generation)
    }
}

fn validate_key(key: &str) -> Result<(), PluginStateError> {
    let reason = if key.is_empty() {
        Some(KeyRejection::Empty)
    } else if key.len() > 128 {
        Some(KeyRejection::TooLong)
    } else {
        key.bytes()
            .enumerate()
            .find(|(_, b)| !b.is_ascii_alphanumeric() && !b"._-".contains(b))
            .map(|(at, byte)| KeyRejection::IllegalCharacter { at, byte })
    };
    match reason {
        Some(reason) => Err(PluginStateError::InvalidKey {
            key: key.into(),
            reason,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;

#[derive(Debug)]
enum StatePhase {
    Registering(Vec<(String, Vec<PluginStateEdit>)>),
    Ready,
}

#[derive(Debug)]
pub(super) struct PluginStateRegistry {
    pub(super) data: PluginState,
    phase: StatePhase,
    source: Option<crate::BlobRef>,
}
impl Default for PluginStateRegistry {
    fn default() -> Self {
        Self {
            data: PluginState::default(),
            phase: StatePhase::Registering(Vec::new()),
            source: None,
        }
    }
}
impl PluginStateRegistry {
    // Hydrate before admission so register calls validate durable membership and
    // size. Replay preserves accepted generations without a late size rejection.
    pub(super) fn registering(snapshot: Option<&PluginState>) -> Self {
        Self {
            data: snapshot.cloned().unwrap_or_default(),
            ..Self::default()
        }
    }
    fn record(&mut self, id: &str, edits: Vec<PluginStateEdit>) {
        if let StatePhase::Registering(log) = &mut self.phase {
            log.push((id.into(), edits));
        }
    }
    pub(super) fn initialize(
        &mut self,
        snapshot: Option<&PluginState>,
    ) -> Result<(), PluginStateError> {
        let StatePhase::Registering(log) = std::mem::replace(&mut self.phase, StatePhase::Ready)
        else {
            unreachable!("initialize once")
        };
        if let Some(snapshot) = snapshot {
            let candidate = Arc::new(Mutex::new(Self {
                data: snapshot.clone(),
                phase: StatePhase::Ready,
                source: None,
            }));
            for (id, edits) in log {
                PluginStateStore::bind(
                    &crate::RuntimeOwner::Session(SessionId::from("")),
                    &id,
                    candidate.clone(),
                )
                .apply(edits)?;
            }
            let mut hydrated = candidate.lock_recover().data.clone();
            for id in self.data.plugins.keys() {
                hydrated.plugins.entry(id.clone()).or_default();
            }
            self.data = hydrated;
            self.source = Some(state_ref(snapshot));
        }
        Ok(())
    }
    pub(super) fn matches_ref(&self, reference: &crate::BlobRef) -> bool {
        self.source.as_ref() == Some(reference) || state_ref(&self.data) == *reference
    }
    pub(super) fn was_hydrated_from(&self, snapshot: &PluginState) -> bool {
        self.source.as_ref() == Some(&state_ref(snapshot)) || self.data == *snapshot
    }
    /// Adopt a recorded head's plugin state as the live state. The recorded
    /// head is durable truth: the drive that committed it owned the head
    /// (FIG-4202), so an accepted write the head does not carry is an
    /// uncommitted tail, and the live state drops it exactly as a cold
    /// rebuild from that head would (ADR 0078 §5). A namespace bound live
    /// but absent from the head stays bound, at its default.
    pub(super) fn hydrate_live(&mut self, snapshot: &PluginState) {
        if self.was_hydrated_from(snapshot) {
            return;
        }
        let empty = PluginNamespaceState::default();
        for (id, live) in &self.data.plugins {
            let recorded = snapshot.plugins.get(id).unwrap_or(&empty);
            if recorded.generation <= live.generation && recorded != live {
                tracing::info!(
                    event = "plugin_state.uncommitted_tail_dropped",
                    plugin_id = %id,
                    live_generation = live.generation,
                    recorded_generation = recorded.generation,
                    "live plugin state adopted a recorded head that does not carry its accepted writes"
                );
            }
        }
        let resident_ids = self.data.plugins.keys().cloned().collect::<Vec<_>>();
        self.data = snapshot.clone();
        for id in resident_ids {
            self.data.plugins.entry(id).or_default();
        }
        self.source = Some(state_ref(snapshot));
    }
}
#[expect(
    clippy::expect_used,
    reason = "`PluginState` is a map of strings to `serde_json::Value`, which MessagePack encodes without a failing case"
)]
fn state_ref(state: &PluginState) -> crate::BlobRef {
    crate::BlobRef::for_content(&rmp_serde::to_vec_named(state).expect("plugin state encodes"))
}
