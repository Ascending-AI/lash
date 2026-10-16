//! [`DurableCommitHook`]: a durable store that runs a test's hook before
//! each owner commit reaches the store it wraps, a candidate's publication
//! through its page store among them.
//!
//! The hook sees the transaction and its label. It may answer an error in
//! the store's place (the commit then applies nothing), hold the commit
//! while the test commits something else, or panic, as an activation that
//! crashes at that commit would. Every other call passes through, so a
//! test that runs over it runs over the real store.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use lash_durable::domain::pages::{
    CandidatePublication, CandidateReceipt, PageStore, RetirementBacklog, RetirementBudget,
    RetirementProgress, RootLease, RootLeaseId, RootLeaseRequest, StoredPage,
};
use lash_durable::domain::{
    ExecKey, ExecRootRow, HydratedGraph, OwnerKey, ProcessActorRow, PromptCallKey,
    PromptSnapshotRow, PromptText, RootOwner, RunFacts, RunSeq, ScopeKey, SessionCloseRow,
    SessionMailbox, TurnEnd, TurnNamespace, TurnRow, WaitId, WaitRow,
    checkpoint_graph::{DecodeLimits, NodeId},
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableStore, Epoch, FormatSet, HeartbeatOutcome, MailCommit,
    MailTx, NodeLease, NodeSpec, Reaped,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ProcessId, SessionId, TurnId};

/// What runs before an owner commit: `Err` answers the commit in the
/// store's place, with nothing applied.
type CommitHook = Arc<
    dyn Fn(&ActorTx, CommitLabel) -> Pin<Box<dyn Future<Output = Result<(), DurableError>> + Send>>
        + Send
        + Sync,
>;

/// A durable store that runs its armed [`CommitHook`] before each owner
/// commit. See the module documentation.
#[derive(Clone)]
pub struct DurableCommitHook {
    inner: Arc<dyn DurableStore>,
    hook: Arc<Mutex<Option<CommitHook>>>,
}

impl DurableCommitHook {
    /// Pass every call through to `inner` until a hook is armed.
    #[must_use]
    pub fn over(inner: Arc<dyn DurableStore>) -> Self {
        Self {
            inner,
            hook: Arc::new(Mutex::new(None)),
        }
    }

    /// Run `hook` before every owner commit from now on. The future it
    /// answers runs before the commit reaches the store: `Err` answers the
    /// commit in the store's place.
    pub fn arm<F, Fut>(&self, hook: F)
    where
        F: Fn(&ActorTx, CommitLabel) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), DurableError>> + Send + 'static,
    {
        let hook: CommitHook = Arc::new(move |tx, label| Box::pin(hook(tx, label)));
        *self.hook.lock_recover() = Some(hook);
    }

    /// Pass every commit through again.
    pub fn disarm(&self) {
        *self.hook.lock_recover() = None;
    }

    /// Runs the armed hook, if any, before `tx` commits under `label`.
    async fn before_commit(&self, tx: &ActorTx, label: CommitLabel) -> Result<(), DurableError> {
        let hook = self.hook.lock_recover().clone();
        match hook {
            Some(hook) => hook(tx, label).await,
            None => Ok(()),
        }
    }

    fn page_store(&self) -> Result<Arc<dyn PageStore>, DurableError> {
        self.inner.pages().ok_or_else(|| {
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Unavailable,
                message: "the wrapped store publishes no heap pages".to_owned(),
            })
        })
    }
}

/// The wrapped store's page publication, a candidate's commit under the
/// hook as every owner commit is; leases, reads and retirement pass
/// through.
#[async_trait::async_trait]
impl PageStore for DurableCommitHook {
    async fn acquire_root_lease(
        &self,
        request: RootLeaseRequest,
    ) -> Result<RootLease, DurableError> {
        self.page_store()?.acquire_root_lease(request).await
    }

    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError> {
        self.page_store()?.renew_root_lease(lease).await
    }

    async fn release_root_lease(&self, lease: RootLeaseId) -> Result<(), DurableError> {
        self.page_store()?.release_root_lease(lease).await
    }

    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        self.page_store()?.read_pages(lease, pages).await
    }

    async fn commit_candidate(
        &self,
        tx: ActorTx,
        candidate: CandidatePublication,
        label: CommitLabel,
    ) -> Result<CandidateReceipt, DurableError> {
        let pages = self.page_store()?;
        self.before_commit(&tx, label).await?;
        pages.commit_candidate(tx, candidate, label).await
    }

    async fn candidate_state(
        &self,
        exec: &ExecKey,
    ) -> Result<Option<(NodeId, lash_durable::domain::checkpoint_graph::CommitId)>, DurableError>
    {
        self.page_store()?.candidate_state(exec).await
    }

    async fn retire_step(
        &self,
        budget: RetirementBudget,
    ) -> Result<RetirementProgress, DurableError> {
        self.page_store()?.retire_step(budget).await
    }

    async fn retirement_backlog(&self) -> Result<RetirementBacklog, DurableError> {
        self.page_store()?.retirement_backlog().await
    }
}

#[async_trait::async_trait]
impl DurableReads for DurableCommitHook {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
        self.inner.run_facts(owner).await
    }

    async fn run_facts_of(&self, owner: &OwnerKey, run: RunSeq) -> Result<RunFacts, DurableError> {
        self.inner.run_facts_of(owner, run).await
    }
    async fn run_facts_within(
        &self,
        owner: &lash_durable::domain::OwnerKey,
        run: Option<lash_durable::domain::RunSeq>,
        max_rows: usize,
        limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
    ) -> Result<lash_durable::domain::RunFacts, DurableError> {
        self.inner
            .run_facts_within(owner, run, max_rows, limits)
            .await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        self.inner.exec_root(exec).await
    }

    async fn cell_roots(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        self.inner.cell_roots(session, run).await
    }
    async fn cell_roots_within(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
        max_rows: usize,
    ) -> Result<Vec<lash_durable::domain::ExecRootRow>, DurableError> {
        self.inner.cell_roots_within(session, run, max_rows).await
    }

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<NodeId>,
        limits: DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        self.inner.hydrate(owner, expected, limits).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
        self.inner.process(process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.live_until_descendants(scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.until_children(scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.inner.session_close(session).await
    }

    async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
        self.inner.ending_scopes(session).await
    }

    async fn session_mailbox(&self, session: &SessionId) -> Result<SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
    }

    async fn prompt_snapshot(
        &self,
        call: &PromptCallKey,
    ) -> Result<Option<PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(&self, hashes: &[String]) -> Result<Vec<PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}

#[async_trait::async_trait]
impl DurableStore for DurableCommitHook {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    /// The wrapped store's page publication under the hook.
    fn pages(&self) -> Option<std::sync::Arc<dyn lash_durable::domain::pages::PageStore>> {
        self.inner.pages()?;
        Some(Arc::new(self.clone()))
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.inner.register_node(spec).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.inner.heartbeat(node).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.inner.reap(reaper).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.release_node(node).await
    }

    #[cfg(feature = "testing")]
    async fn acquire_idle_for_testing(
        &self,
        node: &lash_durable::NodeLease,
        actor: &lash_durable::ActorKey,
        epoch: lash_durable::Epoch,
    ) -> Result<lash_durable::Epoch, lash_durable::DurableError> {
        self.inner
            .acquire_idle_for_testing(node, actor, epoch)
            .await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.inner.claim(node, limit).await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        self.before_commit(&tx, label).await?;
        self.inner.commit(tx, label).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.inner.commit_mail(tx, label).await
    }

    async fn claim_facts(
        &self,
        node: Option<&lash_durable::Owner>,
        actor: &ActorKey,
    ) -> Result<lash_durable::ClaimFacts, DurableError> {
        self.inner.claim_facts(node, actor).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }

    async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError> {
        self.inner.await_owner_commit(actor).await
    }

    async fn actors_in(
        &self,
        formats: &FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.actors_in(formats, after, limit).await
    }
}
