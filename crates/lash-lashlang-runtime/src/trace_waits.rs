use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash_sansio::ExecutionNodeKind;
use lash_sansio::sync::MutexExt;

type WaitKey = (String, ExecutionNodeKind, u64);

/// Shared occurrence-level wait bookkeeping for process and foreground hosts.
#[derive(Clone, Default)]
pub struct TraceWaitBookkeeping {
    pending: Arc<Mutex<BTreeSet<WaitKey>>>,
}

impl TraceWaitBookkeeping {
    pub fn mark_waiting(&self, node_id: &str, kind: ExecutionNodeKind, occurrence: u64) {
        self.pending
            .lock_recover()
            .insert((node_id.to_owned(), kind, occurrence));
    }

    pub fn is_waiting(&self, node_id: &str, kind: ExecutionNodeKind, occurrence: u64) -> bool {
        self.pending
            .lock_recover()
            .contains(&(node_id.to_owned(), kind, occurrence))
    }

    /// Returns whether an observed wait was removed. Cancellation emits a
    /// cancelled resolution only for that case.
    pub fn finish(&self, node_id: &str, kind: ExecutionNodeKind, occurrence: u64) -> bool {
        self.pending
            .lock_recover()
            .remove(&(node_id.to_owned(), kind, occurrence))
    }

    pub fn is_empty(&self) -> bool {
        self.pending.lock_recover().is_empty()
    }
}
