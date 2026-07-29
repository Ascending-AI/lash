//! Shared fixtures for the conformance suites: paired handles opened
//! against the same durable backing store, used by the `*_reopenable`
//! suite variants.

use super::*;

pub(crate) fn durable_turn_scope(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> ExecutionScope {
    ExecutionScope::turn(session_id, turn_id)
}

pub(crate) fn durable_turn_address(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> crate::TurnAddress {
    crate::TurnAddress::new(session_id, turn_id)
}

/// A pair of [`ProcessRegistry`] handles opened against the same durable
/// backing store.
pub struct ReopenableProcessRegistry {
    pub open: Arc<dyn ProcessRegistry>,
    pub reopen: Arc<dyn ProcessRegistry>,
}

/// A pair of [`RuntimePersistence`] handles opened against the same durable
/// backing store.
pub struct ReopenableRuntimePersistence {
    pub open: Arc<dyn RuntimePersistence>,
    pub reopen: Arc<dyn RuntimePersistence>,
}

/// A pair of [`AttachmentStore`](crate::AttachmentStore) handles opened against
/// the same durable backing store.
pub struct ReopenableAttachmentStore {
    pub open: Arc<dyn crate::AttachmentStore>,
    pub reopen: Arc<dyn crate::AttachmentStore>,
}

/// A pair of [`TriggerStore`](crate::TriggerStore) handles opened against
/// the same durable backing store.
pub struct ReopenableTriggerStore {
    pub open: Arc<dyn crate::TriggerStore>,
    pub reopen: Arc<dyn crate::TriggerStore>,
}
