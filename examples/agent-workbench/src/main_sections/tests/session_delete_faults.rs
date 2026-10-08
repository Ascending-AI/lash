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

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        self.inner.run_records(owner).await
    }

    async fn run_record_owners(&self, actor: &ActorKey) -> Result<Vec<OwnerKey>, DurableError> {
        self.inner.run_record_owners(actor).await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        self.inner.snapshot(exec).await
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

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.inner.park_events(after, limit).await
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

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
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
    fn process_registry(&self) -> Arc<dyn lash::process::ProcessRegistry> {
        self.inner.process_registry()
    }
    fn trigger_store(&self) -> Arc<dyn lash::triggers::TriggerStore> {
        self.inner.trigger_store()
    }
    fn process_env_store(&self) -> Arc<dyn lash::persistence::ProcessExecutionEnvStore> {
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
    fn recovery_leader(&self) -> Arc<dyn lash::persistence::RecoveryLeaderStore> {
        self.inner.recovery_leader()
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
