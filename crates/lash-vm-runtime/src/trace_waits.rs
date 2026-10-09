use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash_sansio::WorkflowOccurrence;
use lash_sansio::sync::MutexExt;

/// Shared occurrence-level wait bookkeeping for process and foreground hosts.
/// An occurrence is one run of one site: occurrences count per site.
#[derive(Clone, Default)]
pub struct TraceWaitBookkeeping {
    pending: Arc<Mutex<BTreeSet<WorkflowOccurrence>>>,
}

impl TraceWaitBookkeeping {
    pub fn mark_waiting(&self, at: &WorkflowOccurrence) {
        self.pending.lock_recover().insert(at.clone());
    }

    pub fn is_waiting(&self, at: &WorkflowOccurrence) -> bool {
        self.pending.lock_recover().contains(at)
    }

    /// Returns whether an observed wait was removed. Cancellation emits a
    /// cancelled resolution only for that case.
    pub fn finish(&self, at: &WorkflowOccurrence) -> bool {
        self.pending.lock_recover().remove(at)
    }

    pub fn is_empty(&self) -> bool {
        self.pending.lock_recover().is_empty()
    }
}
