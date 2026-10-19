//! Faults at the public store ports of the real SQLite workbench.
//! Close mail can be refused before commit or lose its acknowledgement after
//! the actor closed. Catalog reads can withhold a committed tombstone, answering
//! an unavailable read rather than inventing a live or absent session.
use super::*;
use lash::durable::domain::*;
use lash::durable::*;
use lash::persistence::{
    DeploymentStore, DeploymentStoreDecorator, RuntimeStoreDecorator, SessionLookup, StoreError,
};
use lash::{ProcessId, SessionId};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseFault {
    Refuse,
    CommitThenRefuse,
}

pub(crate) struct SessionDeleteFaults {
    session: lash::SessionId,
    actor: ActorKey,
    close: Mutex<Option<CloseFault>>,
    hide_tombstone: AtomicBool,
    refused_close: AtomicBool,
    tombstone: watch::Sender<bool>,
    hidden_read: watch::Sender<bool>,
}

impl SessionDeleteFaults {
    pub(crate) fn new(session: lash::SessionId) -> Arc<Self> {
        Arc::new(Self {
            actor: ActorKey::session(session.as_str()).expect("a session actor"),
            session,
            close: Mutex::new(None),
            hide_tombstone: AtomicBool::new(false),
            refused_close: AtomicBool::new(false),
            tombstone: watch::channel(false).0,
            hidden_read: watch::channel(false).0,
        })
    }

    pub(crate) fn refuse_close(&self, fault: CloseFault) {
        *self.close.lock_recover() = Some(fault);
    }

    pub(crate) fn refused_close(&self) -> bool {
        self.refused_close.load(Ordering::SeqCst)
    }

    pub(crate) fn withhold_tombstone(&self) {
        self.hide_tombstone.store(true, Ordering::SeqCst);
    }

    pub(crate) fn reveal_tombstone(&self) {
        self.hide_tombstone.store(false, Ordering::SeqCst);
    }

    pub(crate) async fn wait_for_tombstone(&self) {
        self.tombstone
            .subscribe()
            .wait_for(|committed| *committed)
            .await
            .expect("the store reports the tombstone commit");
    }

    pub(crate) async fn wait_for_hidden_read(&self) {
        self.hidden_read
            .subscribe()
            .wait_for(|hidden| *hidden)
            .await
            .expect("the catalog reports a withheld tombstone read");
    }

    pub(crate) fn install(
        self: &Arc<Self>,
        inner: Arc<dyn lash::StoreSet>,
    ) -> Arc<dyn lash::StoreSet> {
        Arc::new(DeleteStores {
            durable: Arc::new(DeleteDurableStore {
                inner: inner.durable_store(),
                faults: Arc::clone(self),
            }),
            catalog: Arc::new(DeleteCatalog {
                inner: inner.session_store_factory(),
                faults: Arc::clone(self),
            }),
            inner,
        })
    }
}

fn close_refused() -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: "the close-mail store refused the request".to_string(),
    })
}

struct DeleteCatalog {
    inner: Arc<dyn DeploymentStore>,
    faults: Arc<SessionDeleteFaults>,
}

#[async_trait::async_trait]
impl RuntimeStoreDecorator for DeleteCatalog {
    type Inner = dyn DeploymentStore;
    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn lookup_session(&self, session: &lash::SessionId) -> Result<SessionLookup, StoreError> {
        let lookup = self.inner.lookup_session(session).await?;
        if session == self.faults.session
            && self.faults.hide_tombstone.load(Ordering::SeqCst)
            && matches!(lookup, SessionLookup::Deleted)
        {
            self.faults.hidden_read.send_replace(true);
            return Err(StoreError::Contended);
        }
        Ok(lookup)
    }
}
impl DeploymentStoreDecorator for DeleteCatalog {}

struct DeleteDurableStore {
    inner: Arc<dyn DurableStore>,
    faults: Arc<SessionDeleteFaults>,
}

#[async_trait::async_trait]
impl DurableReads for DeleteDurableStore {
    async fn model_calls(
        &self,
        session: &lash::SessionId,
        owner: &str,
    ) -> Result<Vec<lash::turn::ModelCallRecord>, DurableError> {
        self.inner.model_calls(session, owner).await
    }
    async fn model_call_pins(
        &self,
        session: &lash::SessionId,
        owner: &str,
    ) -> Result<Vec<lash::durable::domain::ModelCallPinRow>, DurableError> {
        self.inner.model_call_pins(session, owner).await
    }

    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<lash::durable::domain::TurnNamespace>, DurableError> {
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

    async fn run_facts_of(
        &self,
        owner: &OwnerKey,
        run: lash::durable::domain::RunSeq,
    ) -> Result<RunFacts, DurableError> {
        self.inner.run_facts_of(owner, run).await
    }

    async fn run_facts_within(
        &self,
        owner: &lash::durable::domain::OwnerKey,
        run: Option<lash::durable::domain::RunSeq>,
        max_rows: usize,
        limits: lash::persistence::GraphDecodeLimits,
    ) -> Result<lash::durable::domain::RunFacts, DurableError> {
        self.inner
            .run_facts_within(owner, run, max_rows, limits)
            .await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        self.inner.exec_root(exec).await
    }

    async fn cell_roots(
        &self,
        session: &lash::SessionId,
        run: &lash::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        self.inner.cell_roots(session, run).await
    }

    async fn cell_roots_within(
        &self,
        session: &lash::SessionId,
        run: &lash::TurnId,
        max_rows: usize,
    ) -> Result<Vec<lash::durable::domain::ExecRootRow>, DurableError> {
        self.inner.cell_roots_within(session, run, max_rows).await
    }

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<lash::durable::domain::checkpoint_graph::NodeId>,
        limits: lash::durable::domain::checkpoint_graph::DecodeLimits,
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

    async fn session_mailbox(
        &self,
        session: &SessionId,
    ) -> Result<lash::durable::domain::SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash::durable::domain::PromptCallKey,
    ) -> Result<Option<lash::durable::domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash::durable::domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}

#[async_trait::async_trait]
impl DurableStore for DeleteDurableStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    fn pages(&self) -> Option<std::sync::Arc<dyn lash::durable::domain::pages::PageStore>> {
        self.inner.pages()
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

    async fn acquire_idle_for_testing(
        &self,
        node: &lash::durable::NodeLease,
        actor: &lash::durable::ActorKey,
        epoch: lash::durable::Epoch,
    ) -> Result<lash::durable::Epoch, lash::durable::DurableError> {
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

    async fn live_decodes(&self) -> Result<Vec<Vec<lash::durable::FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(
        &self,
        actor: &ActorKey,
        epoch: lash::durable::Epoch,
    ) -> Result<ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let closes_session =
            tx.actor() == &self.faults.actor && label == CommitLabel::SESSION_CLOSE_TOMBSTONE;
        let committed = self.inner.commit(tx, label).await?;
        if closes_session {
            self.faults.tombstone.send_replace(true);
        }
        Ok(committed)
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let closing = tx.writes().iter().any(|write| {
            matches!(
                write,
                MailWrite::Append { actor, kind, .. }
                    if actor == &self.faults.actor && kind.as_str() == "session.close"
            )
        });
        let fault = if closing {
            *self.faults.close.lock_recover()
        } else {
            None
        };
        if fault == Some(CloseFault::Refuse) {
            self.faults.close.lock_recover().take();
            self.faults.refused_close.store(true, Ordering::SeqCst);
            return Err(close_refused());
        }
        let committed = self.inner.commit_mail(tx, label).await?;
        if fault == Some(CloseFault::CommitThenRefuse) {
            self.faults.close.lock_recover().take();
            self.faults.wait_for_tombstone().await;
            self.faults.refused_close.store(true, Ordering::SeqCst);
            return Err(close_refused());
        }
        Ok(committed)
    }

    async fn claim_facts(
        &self,
        node: Option<&lash::durable::Owner>,
        actor: &lash::durable::ActorKey,
    ) -> Result<lash::durable::ClaimFacts, DurableError> {
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

struct DeleteStores {
    inner: Arc<dyn lash::StoreSet>,
    durable: Arc<DeleteDurableStore>,
    catalog: Arc<DeleteCatalog>,
}

impl lash::StoreSet for DeleteStores {
    fn durable_store(&self) -> Arc<dyn DurableStore> {
        self.durable.clone()
    }
    fn wake_sink(&self) -> lash::durable::runner::WakeSink {
        self.inner.wake_sink()
    }

    fn node_wakes(&self) -> Option<Arc<dyn lash::durable::NodeWakes>> {
        self.inner.node_wakes()
    }
    fn binding_identity(&self) -> &lash::StoreBindingId {
        self.inner.binding_identity()
    }
    fn clock(&self) -> Arc<dyn lash::runtime::Clock> {
        self.inner.clock()
    }
    fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.catalog.clone()
    }
    fn attachment_referrers(&self) -> Arc<dyn lash::persistence::AttachmentReferrers> {
        self.inner.attachment_referrers()
    }
    fn process_registry(&self) -> Arc<dyn lash::persistence::ProcessRegistry> {
        self.inner.process_registry()
    }
    fn process_authority_reads(&self) -> Arc<dyn lash::persistence::ProcessAuthorityReads> {
        self.inner.process_authority_reads()
    }

    fn process_env_store(&self) -> Arc<dyn lash::persistence::ProcessEnvStore> {
        self.inner.process_env_store()
    }
    fn turn_prelude_store(&self) -> Arc<dyn lash::persistence::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }
    fn tool_material_store(&self) -> Arc<dyn lash::persistence::ToolMaterialStore> {
        self.inner.tool_material_store()
    }
    fn definition_store(&self) -> Arc<dyn lash::persistence::ProcessDefinitionStore> {
        self.inner.definition_store()
    }
    fn attachment_store(&self) -> Arc<dyn lash::persistence::AttachmentStore> {
        self.inner.attachment_store()
    }
    fn module_artifacts(&self) -> Arc<dyn lash::persistence::ModuleArtifactStore> {
        self.inner.module_artifacts()
    }
    fn obligation_ledger(
        &self,
        kind: lash::ObligationKind,
    ) -> Arc<dyn lash::persistence::ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }
    fn artifact_cleanup(&self) -> Arc<dyn lash::persistence::ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }
}
