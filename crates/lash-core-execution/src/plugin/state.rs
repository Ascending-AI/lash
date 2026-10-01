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
pub enum KeyRejection {
    #[error("empty key")]
    Empty,
    #[error("key exceeds 128 bytes")]
    TooLong,
    #[error("illegal byte {byte} at {at}")]
    IllegalCharacter { at: usize, byte: u8 },
}

/// Rejections are atomic: neither values nor generation change.
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
    /// A resident acceptance token. Hydration never reuses a token for changed
    /// content, even when it restores an older checkpoint generation.
    pub fn generation(&self) -> u64 {
        self.state.lock_recover().generation(self.plugin_id())
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
        let accepted_generation = state.generation(self.plugin_id());
        let namespace = state
            .data
            .plugins
            .get_mut(self.plugin_id())
            .expect("bound namespace");
        let removed = namespace.values.contains_key(key);
        if removed {
            let generation = accepted_generation
                .checked_add(1)
                .expect("plugin generation exhausted");
            namespace.values.remove(key);
            namespace.generation = generation;
        }
        let generation = if removed {
            namespace.generation
        } else {
            accepted_generation
        };
        if removed {
            state
                .acceptance_generations
                .insert(self.plugin_id().into(), generation);
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
        let accepted_generation = state.generation(self.plugin_id());
        if let Some(expected) = expected
            && expected != accepted_generation
        {
            return Err(PluginStateError::GenerationConflict {
                expected,
                actual: accepted_generation,
            });
        }
        let recorded_edits = match state.phase {
            StatePhase::Registering(_) => edits.clone(),
            StatePhase::Ready(_) => Vec::new(),
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
        let generation = accepted_generation
            .checked_add(1)
            .expect("plugin generation exhausted");
        namespace.values = values;
        namespace.generation = generation;
        state
            .acceptance_generations
            .insert(self.plugin_id().into(), generation);
        state.record(self.plugin_id(), recorded_edits);
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

pub(super) fn validate_namespace(values: &BTreeMap<String, Value>) -> Result<(), PluginStateError> {
    for (key, value) in values {
        validate_key(key)?;
        let bytes = serde_json::to_vec(value)
            .map_err(|error| PluginStateError::Encode {
                key: key.clone(),
                message: error.to_string(),
            })?
            .len();
        if bytes > VALUE_LIMIT {
            return Err(PluginStateError::ValueTooLarge {
                key: key.clone(),
                bytes,
                limit: VALUE_LIMIT,
            });
        }
    }
    let bytes = serde_json::to_vec(values)
        .map_err(|error| PluginStateError::Encode {
            key: String::new(),
            message: error.to_string(),
        })?
        .len();
    if bytes > STORE_LIMIT {
        return Err(PluginStateError::StoreTooLarge {
            bytes,
            limit: STORE_LIMIT,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[derive(Debug)]
enum StatePhase {
    Registering(Vec<(String, Vec<PluginStateEdit>)>),
    Ready(Vec<(String, Vec<PluginStateEdit>)>),
}

#[derive(Debug)]
pub(super) struct PluginStateRegistry {
    pub(super) data: PluginState,
    phase: StatePhase,
    source: Option<crate::BlobRef>,
    /// Resident guards outlive checkpoint adoption. Checkpoint generations
    /// describe restored data; these tokens must never authorize another value.
    acceptance_generations: BTreeMap<String, u64>,
}
impl Default for PluginStateRegistry {
    fn default() -> Self {
        Self {
            data: PluginState::default(),
            phase: StatePhase::Registering(Vec::new()),
            source: None,
            acceptance_generations: BTreeMap::new(),
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
    /// Materialization writes are replayed over every hydration, so they are
    /// part of it; a write accepted once ready is a tail the source lacks.
    fn record(&mut self, id: &str, edits: Vec<PluginStateEdit>) {
        match &mut self.phase {
            StatePhase::Registering(log) => log.push((id.into(), edits)),
            StatePhase::Ready(_) => self.source = None,
        }
    }
    pub(super) fn initialize(
        &mut self,
        snapshot: Option<&PluginState>,
    ) -> Result<(), PluginStateError> {
        let StatePhase::Registering(log) =
            std::mem::replace(&mut self.phase, StatePhase::Ready(Vec::new()))
        else {
            unreachable!("initialize once")
        };
        if let Some(snapshot) = snapshot {
            let candidate = Arc::new(Mutex::new(Self {
                data: snapshot.clone(),
                phase: StatePhase::Ready(Vec::new()),
                source: None,
                acceptance_generations: BTreeMap::new(),
            }));
            {
                let mut candidate = candidate.lock_recover();
                for (id, namespace) in &self.data.plugins {
                    candidate.data.plugins.entry(id.clone()).or_insert_with(|| {
                        PluginNamespaceState {
                            format_version: namespace.format_version,
                            ..Default::default()
                        }
                    });
                }
            }
            for (id, edits) in &log {
                PluginStateStore::bind(
                    &crate::RuntimeOwner::Session(SessionId::from("")),
                    id,
                    candidate.clone(),
                )
                .apply(edits.clone())?;
            }
            let mut hydrated = candidate.lock_recover().data.clone();
            for (id, namespace) in &self.data.plugins {
                hydrated
                    .plugins
                    .entry(id.clone())
                    .or_insert_with(|| PluginNamespaceState {
                        format_version: namespace.format_version,
                        ..Default::default()
                    });
            }
            self.data = hydrated;
            self.source = Some(state_ref(snapshot));
        }
        self.phase = StatePhase::Ready(log);
        Ok(())
    }
    fn generation(&self, id: &str) -> u64 {
        self.acceptance_generations
            .get(id)
            .copied()
            .unwrap_or(self.data.plugins[id].generation)
    }
    pub(super) fn matches_ref(&self, reference: &crate::BlobRef) -> bool {
        self.source.as_ref() == Some(reference) || state_ref(&self.data) == *reference
    }
    pub(super) fn was_hydrated_from(&self, snapshot: &PluginState) -> bool {
        self.source.as_ref() == Some(&state_ref(snapshot)) || self.data == *snapshot
    }
    /// Reconstruct the head plus accepted materialization edits. Those edits
    /// were validated during materialization; replay changes no acceptance decision.
    #[expect(
        clippy::expect_used,
        reason = "a u64 plugin generation counter cannot be exhausted"
    )]
    fn hydrated_data(&self, snapshot: &PluginState) -> PluginState {
        let mut hydrated = snapshot.clone();
        let log = match &self.phase {
            StatePhase::Registering(log) | StatePhase::Ready(log) => log,
        };
        for (id, edits) in log {
            let format_version = self
                .data
                .plugins
                .get(id)
                .map_or(super::FormatVersion::ONE, |namespace| {
                    namespace.format_version
                });
            let namespace =
                hydrated
                    .plugins
                    .entry(id.clone())
                    .or_insert_with(|| PluginNamespaceState {
                        format_version,
                        ..Default::default()
                    });
            for edit in edits {
                match edit {
                    PluginStateEdit::Set { key, value } => {
                        let mut value = value.clone();
                        value.sort_all_objects();
                        namespace.values.insert(key.clone(), value);
                    }
                    PluginStateEdit::Remove { key } => {
                        namespace.values.remove(key);
                    }
                }
            }
            namespace.generation = namespace
                .generation
                .checked_add(1)
                .expect("plugin generation exhausted");
        }
        for (id, namespace) in &self.data.plugins {
            hydrated
                .plugins
                .entry(id.clone())
                .or_insert_with(|| PluginNamespaceState {
                    format_version: namespace.format_version,
                    ..Default::default()
                });
        }
        hydrated
    }

    /// Adopt the same checkpoint data as a cold materialization, discarding
    /// uncommitted Ready writes. Resident guard tokens remain monotonic and
    /// changing content invalidates guards issued before the adoption.
    #[expect(
        clippy::expect_used,
        reason = "a u64 plugin generation counter cannot be exhausted"
    )]
    pub(super) fn hydrate_live(&mut self, snapshot: &PluginState) {
        if self.was_hydrated_from(snapshot) {
            return;
        }
        let hydrated = self.hydrated_data(snapshot);
        for (id, recorded) in &hydrated.plugins {
            let Some(live) = self.data.plugins.get(id) else {
                continue;
            };
            let mut generation = self.generation(id);
            if recorded != live {
                if recorded.generation <= generation {
                    tracing::info!(
                        event = "plugin_state.uncommitted_tail_dropped",
                        plugin_id = %id,
                        live_generation = generation,
                        recorded_generation = recorded.generation,
                        "live plugin state adopted a recorded head that does not carry its accepted writes"
                    );
                }
                generation = generation
                    .checked_add(1)
                    .expect("plugin generation exhausted");
            }
            self.acceptance_generations
                .insert(id.clone(), generation.max(recorded.generation));
        }
        self.data = hydrated;
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
