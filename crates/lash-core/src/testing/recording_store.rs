//! A recording and fault-injecting decorator over any session store.
//!
//! [`RecordingStore`] wraps the store a test's backend hands out and adds
//! only what a test observes or forces: call counters and one-shot faults on
//! the operations the runtime suites steer. Every persistence semantic stays
//! the backend's own (ADR 0102): the decorator never answers a read or
//! applies a write itself, so a test that runs over it runs over the real
//! store.
//!
//! [`RecordingSessionStoreFactory`] decorates a backend's session catalog the
//! same way, wrapping every store it creates or reopens in a
//! [`RecordingStore`] and keeping the ones it created for the test to read.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_sansio::sync::MutexExt;

use crate::store::{
    RuntimeCommit, RuntimeCommitReceipt, RuntimeStore, RuntimeStoreDecorator, SessionWindowRead,
    StoreError, WindowSelector,
};
use crate::{DeploymentStore, SessionId, SessionStoreCreateRequest};
use lash_core_execution::DeploymentStoreDecorator;

/// A session store with call counters and one-shot faults over the store it
/// wraps. See the module documentation.
pub struct RecordingStore {
    inner: Arc<dyn RuntimeStore>,
    session_id: Mutex<Option<SessionId>>,
    /// Runtime commits the wrapped store applied; an attempt it answered from
    /// an earlier commit's durable receipt is not one.
    pub runtime_commit_count: Mutex<usize>,
    /// Every runtime commit the wrapped store applied, in order: the committed
    /// bytes a test pins.
    runtime_commits: Mutex<Vec<RuntimeCommit>>,
    commit_attempt_count: AtomicUsize,
    load_session_count: AtomicUsize,
    load_session_head_meta_count: AtomicUsize,
    list_queued_work_count: AtomicUsize,
    fail_next_runtime_commit: Mutex<Option<StoreError>>,
    fail_next_end_refused_root: Mutex<Option<StoreError>>,
    inject_turn_cancel_before_next_runtime_commit: Mutex<Option<crate::TurnCancelRequest>>,
    fail_next_load_session_head_meta: AtomicBool,
    fail_load_session_on_call: Mutex<Option<usize>>,
    session_admission_count: AtomicUsize,
    admission_hook: Mutex<Option<AdmissionHook>>,
    forged_head: Mutex<Option<crate::SessionHeadMeta>>,
    attachment_intents: Mutex<Vec<crate::store::AttachmentIntent>>,
}

/// A hook a test runs as the next admission reaches the store.
pub type AdmissionHook = Arc<dyn Fn() + Send + Sync>;

impl RecordingStore {
    /// Record over `inner`, a store the test's backend opened.
    pub fn over(inner: Arc<dyn RuntimeStore>) -> Self {
        Self {
            inner,
            session_id: Mutex::new(None),
            runtime_commit_count: Mutex::new(0),
            runtime_commits: Mutex::new(Vec::new()),
            commit_attempt_count: AtomicUsize::new(0),
            load_session_count: AtomicUsize::new(0),
            load_session_head_meta_count: AtomicUsize::new(0),
            list_queued_work_count: AtomicUsize::new(0),
            fail_next_runtime_commit: Mutex::new(None),
            fail_next_end_refused_root: Mutex::new(None),
            inject_turn_cancel_before_next_runtime_commit: Mutex::new(None),
            fail_next_load_session_head_meta: AtomicBool::new(false),
            fail_load_session_on_call: Mutex::new(None),
            session_admission_count: AtomicUsize::new(0),
            admission_hook: Mutex::new(None),
            forged_head: Mutex::new(None),
            attachment_intents: Mutex::new(Vec::new()),
        }
    }

    pub fn over_session(inner: Arc<dyn RuntimeStore>, session_id: SessionId) -> Self {
        let store = Self::over(inner);
        *store.session_id.lock_recover() = Some(session_id);
        store
    }

    /// Every runtime commit the wrapped store applied, in commit order.
    pub fn runtime_commits(&self) -> Vec<RuntimeCommit> {
        self.runtime_commits.lock_recover().clone()
    }

    /// Every attachment write intent this store began, in order: the owner
    /// each write was attributed to when it started.
    pub fn attachment_intents(&self) -> Vec<crate::store::AttachmentIntent> {
        self.attachment_intents.lock_recover().clone()
    }

    /// Answer every later head read with `head` until a commit through this
    /// store lands: `load_session_head_meta` returns it, and `load_session`
    /// returns the stored session under its revision, config, current frame,
    /// checkpoint ref and leaf.
    ///
    /// This is a read-side double for a head the durable store cannot hold on
    /// its own — a changed checkpoint ref or leaf under an unchanged revision —
    /// so a test can drive the runtime's freshness decision on it. A head a
    /// real writer could produce belongs in a real commit instead
    /// ([`super::runtime_helpers::advance_session_head`]).
    pub fn forge_session_head(&self, head: crate::SessionHeadMeta) {
        *self.forged_head.lock_recover() = Some(head);
    }

    /// Session admissions (admit-and-bind) through this store.
    pub fn session_admission_count(&self) -> usize {
        self.session_admission_count.load(Ordering::SeqCst)
    }

    /// Run `hook` as the next root or checkpoint admission reaches the
    /// store, before the wrapped store admits.
    pub fn set_admission_hook(&self, hook: AdmissionHook) {
        *self.admission_hook.lock_recover() = Some(hook);
    }

    fn run_admission_hook(&self) {
        let hook = self.admission_hook.lock_recover().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// The store this one records over.
    pub fn inner(&self) -> &Arc<dyn RuntimeStore> {
        &self.inner
    }

    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id.lock_recover().clone()
    }

    /// Runtime commits attempted through this store, accepted or not.
    pub fn commit_write_transaction_count(&self) -> usize {
        self.commit_attempt_count.load(Ordering::SeqCst)
    }

    pub fn load_session_count(&self) -> usize {
        self.load_session_count.load(Ordering::SeqCst)
    }

    pub fn load_session_head_meta_count(&self) -> usize {
        self.load_session_head_meta_count.load(Ordering::SeqCst)
    }

    pub fn list_queued_work_count(&self) -> usize {
        self.list_queued_work_count.load(Ordering::SeqCst)
    }

    /// Refuse the next runtime commit with `error`, before the wrapped store
    /// sees it.
    pub fn fail_next_runtime_commit(&self, error: StoreError) {
        *self.fail_next_runtime_commit.lock_recover() = Some(error);
    }

    /// Refuse the next root-end write of a refused run with `error`, before
    /// the wrapped store sees it: the refused run fails between meeting its
    /// refusal and writing its end.
    pub fn fail_next_end_refused_root(&self, error: StoreError) {
        *self.fail_next_end_refused_root.lock_recover() = Some(error);
    }

    /// Record `request` on the wrapped store immediately before the next
    /// runtime commit reaches it: a cancel that races the commit and lands
    /// first.
    pub fn inject_turn_cancel_before_next_runtime_commit(&self, request: crate::TurnCancelRequest) {
        *self
            .inject_turn_cancel_before_next_runtime_commit
            .lock_recover() = Some(request);
    }

    pub fn fail_next_load_session_head_meta(&self) {
        self.fail_next_load_session_head_meta
            .store(true, Ordering::SeqCst);
    }

    /// Fail the `call`-th `load_session` (1-based, counting every call).
    pub fn fail_load_session_on_call(&self, call: usize) {
        *self.fail_load_session_on_call.lock_recover() = Some(call);
    }
}

#[async_trait::async_trait]
impl RuntimeStoreDecorator for RecordingStore {
    type Inner = dyn RuntimeStore;
    async fn admit_root(
        &self,
        request: &crate::store::AdmitRootRequest,
    ) -> Result<Option<crate::store::RootAdmission>, StoreError> {
        self.run_admission_hook();
        self.inner.admit_root(request).await
    }

    async fn admit_at_checkpoint(
        &self,
        request: &crate::store::CheckpointAdmissionRequest,
    ) -> Result<crate::store::CheckpointAdmission, StoreError> {
        self.run_admission_hook();
        self.inner.admit_at_checkpoint(request).await
    }

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: WindowSelector,
    ) -> Result<Option<SessionWindowRead>, StoreError> {
        let call = self.load_session_count.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut fail_on = self.fail_load_session_on_call.lock_recover();
            if fail_on.is_some_and(|fail_on| fail_on == call) {
                fail_on.take();
                return Err(StoreError::Backend(
                    "injected load-session failure".to_string(),
                ));
            }
        }
        let read = self.inner.load_session_window(session_id, selector).await?;
        let forged = self.forged_head.lock_recover().clone();
        let Some(head) = forged else {
            return Ok(read);
        };
        let Some(mut read) = read else {
            return Ok(None);
        };
        read.head_revision = head.head_revision;
        read.config = head.config.clone();
        read.current_frame_node_id = head.current_frame_node_id.clone();
        read.checkpoint_ref = head.checkpoint_ref.clone();
        if read.window.leaf_node_id != head.leaf_node_id {
            read.window = crate::SessionGraph::from_shared_nodes(
                read.window.nodes.clone(),
                head.leaf_node_id.clone(),
            )
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        Ok(Some(read))
    }

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::SessionHeadMeta>, StoreError> {
        self.load_session_head_meta_count
            .fetch_add(1, Ordering::SeqCst);
        if self
            .fail_next_load_session_head_meta
            .swap(false, Ordering::SeqCst)
        {
            return Err(StoreError::Backend(
                "injected load-session-head failure".to_string(),
            ));
        }
        let forged = self.forged_head.lock_recover().clone();
        match forged {
            Some(head) => Ok(Some(head)),
            None => self.inner.load_session_head_meta(session_id).await,
        }
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        self.commit_attempt_count.fetch_add(1, Ordering::SeqCst);
        let injected_failure = self.fail_next_runtime_commit.lock_recover().take();
        if let Some(error) = injected_failure {
            return Err(error);
        }
        let injected_cancel = self
            .inject_turn_cancel_before_next_runtime_commit
            .lock_recover()
            .take();
        if let Some(request) = injected_cancel {
            self.inner.record_turn_cancel_request(request).await?;
        }
        let applied = commit.clone();
        let receipt = self.inner.commit_runtime_state(commit).await?;
        self.forged_head.lock_recover().take();
        if !receipt.receipt_replayed {
            *self.runtime_commit_count.lock_recover() += 1;
            self.runtime_commits.lock_recover().push(applied);
        }
        Ok(receipt)
    }

    async fn end_refused_root(
        &self,
        session_id: &SessionId,
        root: &crate::TurnId,
        refusal: &crate::RuntimeError,
        at_ms: u64,
    ) -> Result<Option<crate::store::RootTerminal>, StoreError> {
        let injected_failure = self.fail_next_end_refused_root.lock_recover().take();
        if let Some(error) = injected_failure {
            return Err(error);
        }
        self.inner
            .end_refused_root(session_id, root, refusal, at_ms)
            .await
    }

    async fn begin_attachment_write(
        &self,
        intent: crate::store::AttachmentIntent,
    ) -> Result<crate::store::AttachmentWriteFence, StoreError> {
        self.attachment_intents.lock_recover().push(intent.clone());
        self.inner.begin_attachment_write(intent).await
    }

    async fn admit_session(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<crate::store::SessionAdmission, StoreError> {
        self.session_admission_count.fetch_add(1, Ordering::SeqCst);
        *self.session_id.lock_recover() = Some(request.session_id.clone());
        self.inner.admit_session(request).await
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        self.list_queued_work_count.fetch_add(1, Ordering::SeqCst);
        self.inner.list_queued_work(session_id).await
    }
}

/// A session catalog that wraps every store of the catalog it decorates in a
/// [`RecordingStore`], and keeps the ones it created. See the module
/// documentation.
///
/// A session's first create or open is wrapped once and every later open of
/// the same session hands back that wrapper, so a fault armed on
/// [`Self::store_for`] reaches whichever runtime holds the session.
#[derive(Clone)]
pub struct RecordingSessionStoreFactory {
    inner: Arc<dyn DeploymentStore>,
    stores: Arc<Mutex<WrappedStores>>,
    fail_next_delete: Arc<Mutex<Option<DeleteFailure>>>,
}

/// A storage delete's injected stop, with its partial report.
type DeleteFailure = crate::store::MaintenanceFailure<crate::store::SessionBlobReclaimReport>;

/// The per-session wrappers a [`RecordingSessionStoreFactory`] handed out.
type WrappedStores = Vec<(SessionId, Arc<RecordingStore>)>;

impl RecordingSessionStoreFactory {
    /// Record over `inner`, a backend's session catalog.
    pub fn over(inner: Arc<dyn DeploymentStore>) -> Self {
        Self {
            inner,
            stores: Arc::new(Mutex::new(Vec::new())),
            fail_next_delete: Arc::new(Mutex::new(None)),
        }
    }

    /// Stop the next session storage delete with `failure`, before the inner
    /// catalog deletes anything.
    pub fn fail_next_delete(&self, failure: DeleteFailure) {
        *self.fail_next_delete.lock_recover() = Some(failure);
    }

    /// Every store this catalog wrapped, in first-seen order.
    pub fn stores(&self) -> Vec<Arc<RecordingStore>> {
        self.stores
            .lock_recover()
            .iter()
            .map(|(_, store)| Arc::clone(store))
            .collect()
    }

    /// The wrapper over `session_id`'s store, once the session was created or
    /// opened through this catalog.
    pub fn store_for(&self, session_id: &SessionId) -> Option<Arc<RecordingStore>> {
        self.stores
            .lock_recover()
            .iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, store)| Arc::clone(store))
    }

    fn record(&self, session_id: &SessionId) -> Arc<RecordingStore> {
        let mut stores = self.stores.lock_recover();
        if let Some((_, recorded)) = stores.iter().find(|(id, _)| id == session_id) {
            return Arc::clone(recorded);
        }
        let inner: Arc<dyn RuntimeStore> = self.inner.clone();
        let recorded = Arc::new(RecordingStore::over(inner));
        *recorded.session_id.lock_recover() = Some(session_id.clone());
        stores.push((session_id.clone(), Arc::clone(&recorded)));
        recorded
    }
}

#[async_trait::async_trait]
impl RuntimeStoreDecorator for RecordingSessionStoreFactory {
    type Inner = dyn DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_root(
        &self,
        request: &crate::store::AdmitRootRequest,
    ) -> Result<Option<crate::store::RootAdmission>, StoreError> {
        self.record(request.session_id()).admit_root(request).await
    }

    async fn admit_at_checkpoint(
        &self,
        request: &crate::store::CheckpointAdmissionRequest,
    ) -> Result<crate::store::CheckpointAdmission, StoreError> {
        self.record(request.session_id())
            .admit_at_checkpoint(request)
            .await
    }

    async fn admit_session(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<crate::store::SessionAdmission, StoreError> {
        self.record(&request.session_id)
            .admit_session(request)
            .await
    }

    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: WindowSelector,
    ) -> Result<Option<SessionWindowRead>, StoreError> {
        self.record(session_id)
            .load_session_window(session_id, selector)
            .await
    }

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::SessionHeadMeta>, StoreError> {
        self.record(session_id)
            .load_session_head_meta(session_id)
            .await
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        self.record(&commit.session_id)
            .commit_runtime_state(commit)
            .await
    }

    async fn begin_attachment_write(
        &self,
        intent: crate::store::AttachmentIntent,
    ) -> Result<crate::store::AttachmentWriteFence, StoreError> {
        self.record(&intent.session_id)
            .begin_attachment_write(intent)
            .await
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        self.record(session_id).list_queued_work(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        if let Some(failure) = self.fail_next_delete.lock_recover().take() {
            return Err(failure);
        }
        self.inner.delete_session(session_id).await
    }
}

#[async_trait::async_trait]
impl DeploymentStoreDecorator for RecordingSessionStoreFactory {
    async fn reclaim_retained_evidence(
        &self,
        bound: crate::store::RetentionBound,
    ) -> crate::store::MaintenanceResult<crate::store::RetentionReport> {
        self.inner.reclaim_retained_evidence(bound).await
    }
}
