//! Host-mediated plugin state and its deterministic checkpoint representation.
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const VALUE_LIMIT: usize = 32 * 1024;
const STORE_LIMIT: usize = 128 * 1024;

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

/// A deterministic rejection of a plugin-state key.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyRejection {
    #[error("empty key")]
    Empty,
    #[error("key exceeds 128 bytes")]
    TooLong,
    #[error("illegal byte {byte} at {at}")]
    IllegalCharacter { at: usize, byte: u8 },
}

/// Rejections are atomic: neither values nor generation change.
#[derive(Debug, thiserror::Error)]
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
    #[error("cannot encode `{key}`: {source}")]
    Encode {
        key: String,
        source: serde_json::Error,
    },
    #[error("cannot decode `{key}`: {source}")]
    Decode {
        key: String,
        source: serde_json::Error,
    },
    #[error("generation conflict: expected {expected}, actual {actual}")]
    GenerationConflict { expected: u64, actual: u64 },
}

impl From<PluginStateError> for super::PluginError {
    fn from(error: PluginStateError) -> Self {
        Self::Session(error.to_string())
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
    session_id: Arc<str>,
    plugin_id: Arc<str>,
    state: Arc<Mutex<PluginStateRegistry>>,
}

impl std::fmt::Debug for PluginStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginStateStore")
            .field("session_id", &self.session_id)
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PluginStateStore {
    pub(super) fn bind(
        session_id: &str,
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
            session_id: session_id.into(),
            plugin_id: plugin_id.into(),
            state,
        }
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
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
                    source,
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
            source,
        })?;
        self.set(key, value)
    }
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
                PluginStateStore::bind("", &id, candidate.clone()).apply(edits)?;
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
    pub(super) fn hydrate_live(
        &mut self,
        snapshot: &PluginState,
    ) -> Result<(), super::PluginError> {
        if self.was_hydrated_from(snapshot) {
            return Ok(());
        }
        for (id, live) in &self.data.plugins {
            let incoming = snapshot.plugins.get(id).cloned().unwrap_or_default();
            if incoming.generation < live.generation
                || (incoming.generation == live.generation && incoming.values != live.values)
            {
                return Err(super::PluginError::Session(format!(
                    "plugin state for `{id}` would rewind accepted writes; rebuild the session from its durable checkpoint"
                )));
            }
        }
        let resident_ids = self.data.plugins.keys().cloned().collect::<Vec<_>>();
        self.data = snapshot.clone();
        for id in resident_ids {
            self.data.plugins.entry(id).or_default();
        }
        self.source = Some(state_ref(snapshot));
        Ok(())
    }
}
fn state_ref(state: &PluginState) -> crate::BlobRef {
    crate::BlobRef::for_content(&rmp_serde::to_vec_named(state).expect("plugin state encodes"))
}
