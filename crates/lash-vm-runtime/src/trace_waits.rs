use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use lash_sansio::{ExecutionNodeKind, WorkflowSiteRef};

type WaitKey = (WorkflowSiteRef, ExecutionNodeKind, u64);

/// Shared occurrence-level wait bookkeeping for process and foreground hosts.
/// An occurrence is one run of one site: occurrences count per site.
#[derive(Clone, Default)]
pub struct TraceWaitBookkeeping {
    pending: Arc<Mutex<BTreeSet<WaitKey>>>,
}

fn key(site: &lash_vm::LashVmExecutionSite, occurrence: u64) -> WaitKey {
    (site.site_ref(), site.node_kind, occurrence)
}

impl TraceWaitBookkeeping {
    pub fn mark_waiting(&self, site: &lash_vm::LashVmExecutionSite, occurrence: u64) {
        self.pending.lock_recover().insert(key(site, occurrence));
    }

    pub fn is_waiting(&self, site: &lash_vm::LashVmExecutionSite, occurrence: u64) -> bool {
        self.pending.lock_recover().contains(&key(site, occurrence))
    }

    /// Returns whether an observed wait was removed. Cancellation emits a
    /// cancelled resolution only for that case.
    pub fn finish(&self, site: &lash_vm::LashVmExecutionSite, occurrence: u64) -> bool {
        self.pending.lock_recover().remove(&key(site, occurrence))
    }

    pub fn is_empty(&self) -> bool {
        self.pending.lock_recover().is_empty()
    }
}
