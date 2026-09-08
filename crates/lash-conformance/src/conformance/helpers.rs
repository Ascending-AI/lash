//! Shared fixtures for the conformance suites: paired handles opened
//! against the same durable backing store, used by the `*_reopenable`
//! suite variants.

use super::*;

pub(crate) fn assert_fresh_instances<T: ?Sized>(left: &Arc<T>, right: &Arc<T>, suite: &str) {
    assert!(
        !Arc::ptr_eq(left, right),
        "{suite} factory reused one Arc across conformance roles"
    );
}

/// A pair of [`ProcessRegistry`] handles opened against the same durable
/// backing store.
pub struct ReopenableProcessRegistry {
    pub open: Arc<dyn crate::ConformanceProcessRegistry>,
    pub reopen: Arc<dyn crate::ConformanceProcessRegistry>,
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

/// Push an unpersisted event node onto `state`'s active path and make it the
/// resident leaf. Pair with [`commit_conformance_state`] to advance a session's
/// durable head from outside any runtime.
pub(crate) use lash_core::testing::store_fixtures::{
    append_conformance_event_node, bind_conformance_session, commit_conformance_state,
    durable_turn_address, durable_turn_scope,
};
