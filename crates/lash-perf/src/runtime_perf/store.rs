use lash_sansio::sync::MutexExt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use lash_core::store::{RuntimeCommitReceipt, RuntimePersistenceDecorator};
use lash_core::{
    RuntimeCommit, RuntimePersistence, SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};

/// Runtime persistence with the production in-memory semantics plus the one
/// counter the perf report cannot obtain from the public persistence traits.
pub(crate) struct RuntimePerfStore {
    inner: Arc<dyn RuntimePersistence>,
    committed_node_ids: Mutex<HashSet<String>>,
}

impl RuntimePerfStore {
    fn wrap(inner: Arc<dyn RuntimePersistence>) -> Self {
        Self {
            inner,
            committed_node_ids: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn graph_node_count(&self) -> usize {
        self.committed_node_ids.lock_recover().len()
    }
}

impl Default for RuntimePerfStore {
    fn default() -> Self {
        Self::wrap(Arc::new(
            lash_core::facade_support::InMemorySessionStore::default(),
        ))
    }
}

#[async_trait::async_trait]
impl RuntimePersistenceDecorator for RuntimePerfStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        let node_ids = commit
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let receipt = self.inner.commit_runtime_state(commit).await?;
        self.committed_node_ids.lock_recover().extend(node_ids);
        Ok(receipt)
    }
}

#[derive(Clone)]
pub(crate) struct RuntimePerfStoreFactory {
    pub(crate) store: Arc<RuntimePerfStore>,
    child_stores: Arc<Mutex<HashMap<String, Arc<RuntimePerfStore>>>>,
}

impl RuntimePerfStoreFactory {
    pub(crate) fn new(store: Arc<RuntimePerfStore>) -> Self {
        Self {
            store,
            child_stores: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

// The benchmark factory does not own an attachment blob store, so attachment
// reclamation has no roots to discover in a runtime-perf run.
#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for RuntimePerfStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, StoreError> {
        Ok(std::collections::BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &lash_core::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for RuntimePerfStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        if request.parent_session_id().is_none() {
            return Ok(Arc::clone(&self.store) as Arc<dyn RuntimePersistence>);
        }
        let mut stores = self.child_stores.lock_recover();
        let store = stores
            .entry(request.session_id.clone())
            .or_insert_with(|| Arc::new(RuntimePerfStore::default()));
        Ok(Arc::clone(store) as Arc<dyn RuntimePersistence>)
    }

    async fn session_was_deleted(&self, _session_id: &str) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &str,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        Err(lash_core::MaintenanceFailure::failed_before_any_work(
            StoreError::UnsupportedStoreOperation {
                operation: "delete_session",
            },
        ))
    }
}

#[cfg(test)]
mod tests;
