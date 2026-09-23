//! The named process-definition registry (FIG-2995).
//!
//! Under ADR 0095, a *name* is tool input only: a fresh
//! session or a trigger starts a process it did not author by naming a
//! registry entry, never by carrying a definition. ADR 0011 forbids
//! capturing a mutable name into anything durable, so every durable record —
//! a process registration, a trigger subscription, this registry's own rows —
//! pins a durable [`ProcessDefinitionRef`] and never a name. The registry's
//! rows do carry the name, but only as the fence that makes a registration
//! addressable; consumers resolve the name once, at intent execution, pin the
//! reference, and never consult the name again.
//!
//! The table shape follows ADR 0095's ruling: owner scope, name, revision,
//! definition fingerprint, lifecycle tombstone, change sequence, unique on
//! owner scope and name, written by the `RegisterProcessDefinition` intent
//! under revision-and-fingerprint compare-and-swap. Session-scoped names
//! follow the ADR 0049 deletion frontier; host- and platform-scoped
//! tombstones are never collected (ADR 0067).
//!
//! Lifecycle (FIG-1951 ruling): one `Enabled | Disabled | Tombstoned`
//! enum stored as one column with a paired-nullable delete timestamp, not the
//! two-boolean layout the trigger-subscription table still carries.

use std::collections::BTreeMap;
use std::sync::Mutex;

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

/// The durable lifecycle of one registry slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "lifecycle",
    content = "deleted_at_ms",
    rename_all = "snake_case"
)]
pub enum ProcessDefinitionLifecycle {
    /// The slot exists and a name lookup resolves it.
    Enabled,
    /// The slot exists, resolves, and a host policy may later disable it.
    Disabled,
    /// The name is fenced: still unique on the owner scope, but resolves for
    /// no consumer. `deleted_at_ms` is when the tombstone was taken.
    Tombstoned { deleted_at_ms: u64 },
}

impl ProcessDefinitionLifecycle {
    pub fn resolvable(&self) -> bool {
        !matches!(self, Self::Tombstoned { .. })
    }
}

/// A durable registry row: a pinned definition reference under a byte address.
///
/// `definition` is the pinned [`ProcessDefinitionRef`] and `fingerprint` its
/// derived fingerprint, stored so the accompanying store columns can fence a
/// registration compare-and-swap without re-deriving the hash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessDefinitionRecord {
    /// The owner scope the name was registered under.
    pub owner_scope: crate::TriggerOwnerScope,
    /// The registered name. Durable in this row only as the fence.
    pub name: String,
    /// Monotonic revision of this slot; written under its CAS.
    pub revision: u64,
    /// Derived fingerprint of the pinned reference.
    pub fingerprint: String,
    /// The pinned reference. Consumers never store the name.
    pub definition: crate::ProcessDefinitionRef,
    pub lifecycle: ProcessDefinitionLifecycle,
    /// Owner-namespace change sequence, matching the trigger table's shape.
    pub change_seq: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// The caller's expectation about the slot it updates.
///
/// `Some` means "this registration updates the slot I last observed at
/// revision `expected_revision` whose pinned reference fingerprinted to
/// `expected_fingerprint`". Both must match the live row or the write is
/// refused with [`ProcessDefinitionRegistrationRefusal::ConflictingRevision`]: a stale
/// expected revision, or a take-over of the name whose definition changed
/// under the caller, both stop here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProcessDefinitionExpectation {
    pub expected_revision: u64,
    pub expected_fingerprint: String,
}

impl ProcessDefinitionExpectation {
    /// The expectation of a caller that observed the named slot at exactly
    /// this state.
    pub fn observed(revision: u64, fingerprint: impl Into<String>) -> Self {
        Self {
            expected_revision: revision,
            expected_fingerprint: fingerprint.into(),
        }
    }
}

/// Why a registry write was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessDefinitionRegistrationRefusal {
    /// A name slot exists that the caller's CAS expectation does not match.
    ConflictingRevision {
        owner_scope: String,
        name: String,
        revision: u64,
        fingerprint: String,
        message: String,
    },
}

impl std::fmt::Display for ProcessDefinitionRegistrationRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConflictingRevision {
                owner_scope,
                name,
                revision,
                fingerprint,
                message,
            } => write!(
                formatter,
                "process definition registration for `{owner_scope}/{name}` conflicts with \
                 revision {revision} (fingerprint {fingerprint}): {message}"
            ),
        }
    }
}

impl std::error::Error for ProcessDefinitionRegistrationRefusal {}

impl From<ProcessDefinitionRegistrationRefusal> for crate::PluginError {
    fn from(refusal: ProcessDefinitionRegistrationRefusal) -> Self {
        crate::PluginError::Session(refusal.to_string())
    }
}

/// Result of one compare-and-swap registration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProcessDefinitionRegistration {
    /// This caller installed the record.
    Admitted(Box<ProcessDefinitionRecord>),
    /// Another caller or an earlier redrive installed this exact definition
    /// first; the returned record is the durable row.
    Existing(Box<ProcessDefinitionRecord>),
}

impl ProcessDefinitionRegistration {
    /// The record the registry holds after the write.
    pub fn record(&self) -> &ProcessDefinitionRecord {
        match self {
            Self::Admitted(record) | Self::Existing(record) => record,
        }
    }
}

/// The durable home for named process definitions (FIG-2995).
///
/// Store and durable-substrate implementors provide this trait, shaped like
/// [`crate::TriggerStore`] but with the registry's lifecycle column.
#[async_trait::async_trait]
pub trait ProcessDefinitionRegistry: Send + Sync {
    /// Write one named registration under the owner scope, with the caller's
    /// compare-and-swap expectation.
    ///
    /// `OperationId` is an idempotency key within the owner scope, mirroring
    /// `TriggerCommand` alignment: a redrive repeats the same write and gets
    /// the same durable row. A `None` expectation creates the slot at
    /// revision 1 or coalesces onto an identical existing definition; a
    /// `Some` expectation must match the live revision and fingerprint.
    async fn register_definition(
        &self,
        operation_id: &str,
        owner_scope: crate::TriggerOwnerScope,
        name: &str,
        definition: crate::ProcessDefinitionRef,
        expectation: Option<&ProcessDefinitionExpectation>,
    ) -> Result<ProcessDefinitionRegistration, crate::PluginError>;

    /// List the registry rows of one owner scope, tombstoned slots included
    /// so a dead name's fence stays inspectable.
    async fn list_definitions(
        &self,
        owner_scope: &crate::TriggerOwnerScope,
    ) -> Result<Vec<ProcessDefinitionRecord>, crate::PluginError>;
}

/// Validate a registration name: registry slots share the trigger
/// subscription key's opaque-key namespace so a name can never collide with
/// the store's own plumbing (`lash.internal/...`).
pub fn validate_process_definition_name(name: &str) -> Result<(), crate::PluginError> {
    let name = name.trim();
    if !crate::store::namespace::is_valid_opaque_key(name) {
        return Err(crate::PluginError::Session(
            "process definition registration requires a non-empty definition name".to_string(),
        ));
    }
    if name.starts_with(crate::triggers::INTERNAL_TRIGGER_KEY_PREFIX) {
        return Err(crate::PluginError::Session(format!(
            "process definition name `{name}` uses reserved prefix `{}`",
            crate::triggers::INTERNAL_TRIGGER_KEY_PREFIX
        )));
    }
    Ok(())
}

/// The registry answer is dropped on the floor if the name is fenced or
/// unregistered: the caller refuses the intent, and a name is never carried
/// into a durable record.
pub async fn resolve_named_definition(
    registry: &dyn ProcessDefinitionRegistry,
    session_id: &crate::SessionId,
    name: &str,
) -> Result<Option<ProcessDefinitionRecord>, crate::PluginError> {
    let scope = crate::TriggerOwnerScope::session(session_id.clone());
    let records = registry.list_definitions(&scope).await?;
    Ok(records
        .into_iter()
        .find(|record| record.name == name && record.lifecycle.resolvable()))
}

#[derive(Clone, Debug)]
struct Slot {
    record: ProcessDefinitionRecord,
}

/// In-memory registry used by test fixtures and hosts with no durable store.
pub struct InMemoryProcessDefinitionRegistry {
    slots: Mutex<BTreeMap<(String, String), Slot>>,
    clock: std::sync::Arc<dyn crate::Clock>,
}

impl Default for InMemoryProcessDefinitionRegistry {
    fn default() -> Self {
        Self {
            slots: Mutex::new(BTreeMap::new()),
            clock: std::sync::Arc::new(crate::SystemClock),
        }
    }
}

impl InMemoryProcessDefinitionRegistry {
    pub fn with_clock(clock: std::sync::Arc<dyn crate::Clock>) -> Self {
        Self {
            clock,
            ..Self::default()
        }
    }
}

impl std::fmt::Debug for InMemoryProcessDefinitionRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InMemoryProcessDefinitionRegistry")
    }
}

fn slot_key(owner_scope: &crate::TriggerOwnerScope, name: &str) -> (String, String) {
    (owner_scope.namespace(), name.to_string())
}

#[async_trait::async_trait]
impl ProcessDefinitionRegistry for InMemoryProcessDefinitionRegistry {
    async fn register_definition(
        &self,
        operation_id: &str,
        owner_scope: crate::TriggerOwnerScope,
        name: &str,
        definition: crate::ProcessDefinitionRef,
        expectation: Option<&ProcessDefinitionExpectation>,
    ) -> Result<ProcessDefinitionRegistration, crate::PluginError> {
        validate_process_definition_name(name)?;
        let _ = operation_id;
        let mut slots = self.slots.lock_recover();
        let key = slot_key(&owner_scope, name);
        let now_ms = u64::try_from(self.clock.timestamp_datetime().timestamp_millis()).unwrap_or(0);
        match slots.get_mut(&key) {
            Some(slot) => {
                let record = slot.record.clone();
                if record.definition.names_same_definition(&definition) {
                    return Ok(ProcessDefinitionRegistration::Existing(Box::new(record)));
                }
                if expectation.is_none() {
                    return Err(crate::PluginError::Session(
                        ProcessDefinitionRegistrationRefusal::ConflictingRevision {
                            owner_scope: owner_scope.namespace(),
                            name: name.to_string(),
                            revision: record.revision,
                            fingerprint: record.fingerprint.clone(),
                            message: format!(
                                "name `{name}` is registered at revision {};                                  re-registration requires the caller's revision compare-and-swap",
                                record.revision
                            ),
                        }
                        .to_string(),
                    ));
                }
                if let Some(expectation) = expectation
                    && (record.revision != expectation.expected_revision
                        || record.fingerprint != expectation.expected_fingerprint)
                {
                    return Err(crate::PluginError::Session(
                        ProcessDefinitionRegistrationRefusal::ConflictingRevision {
                            owner_scope: owner_scope.namespace(),
                            name: name.to_string(),
                            revision: record.revision,
                            fingerprint: record.fingerprint.clone(),
                            message: format!(
                                "expected revision {} with fingerprint {}",
                                expectation.expected_revision, expectation.expected_fingerprint
                            ),
                        }
                        .to_string(),
                    ));
                }
                let fingerprint = definition.fingerprint();
                slot.record.definition = definition;
                slot.record.fingerprint = fingerprint;
                slot.record.revision += 1;
                slot.record.lifecycle = ProcessDefinitionLifecycle::Enabled;
                slot.record.updated_at_ms = now_ms;
                slot.record.change_seq += 1;
                Ok(ProcessDefinitionRegistration::Admitted(Box::new(
                    slot.record.clone(),
                )))
            }
            None => {
                if expectation.is_some() {
                    return Err(crate::PluginError::Session(
                        ProcessDefinitionRegistrationRefusal::ConflictingRevision {
                            owner_scope: owner_scope.namespace(),
                            name: name.to_string(),
                            revision: 0,
                            fingerprint: String::new(),
                            message:
                                "no slot exists at this name; a stale expectation cannot create one"
                                    .to_string(),
                        }
                        .to_string(),
                    ));
                }
                let fingerprint = definition.fingerprint();
                let record = ProcessDefinitionRecord {
                    owner_scope,
                    name: name.to_string(),
                    revision: 1,
                    fingerprint: fingerprint.clone(),
                    definition,
                    lifecycle: ProcessDefinitionLifecycle::Enabled,
                    change_seq: 1,
                    created_at_ms: now_ms,
                    updated_at_ms: now_ms,
                };
                slots.insert(
                    key,
                    Slot {
                        record: record.clone(),
                    },
                );
                Ok(ProcessDefinitionRegistration::Admitted(Box::new(record)))
            }
        }
    }

    async fn list_definitions(
        &self,
        owner_scope: &crate::TriggerOwnerScope,
    ) -> Result<Vec<ProcessDefinitionRecord>, crate::PluginError> {
        let slots = self.slots.lock_recover();
        let namespace = owner_scope.namespace();
        Ok(slots
            .iter()
            .filter(|((slot_namespace, _), _)| slot_namespace.as_str() == namespace)
            .map(|(_, slot)| slot.record.clone())
            .collect())
    }
}
