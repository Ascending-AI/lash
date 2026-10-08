//! Holding a session's actor on a [`SimEngine`](crate::backend::SimEngine).
//!
//! A generated world models a queued input the model has not withdrawn yet
//! as pending: no run may take it until the model's next provider turn, or
//! its cancellation, says what becomes of it. The durable engine admits mail
//! as soon as the session's actor runs, so the world holds the actor
//! instead: while a [`SessionHold`] lives, every owner transaction of that
//! session's actor waits before it opens. Producers still write mail, and a
//! host still cancels a pending input through the session store, so what is
//! sent meanwhile stays pending exactly until the hold is released.
//!
//! The same durable port observes the session commits the engine's owners
//! make (`turn.commit`, ADR 0132 §4) into the checkpoint-write collectors a
//! world installs ([`CommitObservers`]): those commits never pass the
//! session factory, so a factory observer alone sees none of them. Nothing
//! else of the store set changes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use lash_durable::domain::{
    DomainWrite, ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow,
    ScopeKey, SessionCloseRow, SnapshotRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx,
    NodeLease, NodeSpec, Reaped, StoreFailure, StoreFailureKind,
};
use lash_sansio::{ProcessId, SessionId, TurnId};
use tokio::sync::watch;

use crate::store::CheckpointWriteCollector;

/// The sessions whose actors are held, by actor key: each hold's release
/// signal.
#[derive(Default)]
pub(crate) struct SessionHolds {
    held: Mutex<BTreeMap<String, watch::Receiver<bool>>>,
}

impl SessionHolds {
    /// Hold `session`'s actor until the returned guard drops. A session
    /// already held keeps its first hold.
    pub(crate) fn hold(self: &Arc<Self>, session: &SessionId) -> Result<SessionHold, String> {
        let actor = ActorKey::session(session.as_str()).map_err(|error| error.to_string())?;
        let (release, released) = watch::channel(false);
        self.held
            .lock_recover()
            .entry(actor.as_str().to_owned())
            .or_insert(released);
        Ok(SessionHold {
            holds: Arc::clone(self),
            actor: actor.as_str().to_owned(),
            release,
        })
    }

    /// Wait while `actor` is held.
    async fn pass(&self, actor: &ActorKey) {
        let held = self.held.lock_recover().get(actor.as_str()).cloned();
        if let Some(mut released) = held {
            // A dropped sender is a released hold too.
            let _ = released.wait_for(|released| *released).await;
        }
    }
}

/// A held session actor, released when dropped.
pub struct SessionHold {
    holds: Arc<SessionHolds>,
    actor: String,
    release: watch::Sender<bool>,
}

impl std::fmt::Debug for SessionHold {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionHold")
            .field("actor", &self.actor)
            .finish_non_exhaustive()
    }
}

impl Drop for SessionHold {
    fn drop(&mut self) {
        let mut held = self.holds.held.lock_recover();
        // Only the hold that installed the entry removes it.
        if held
            .get(&self.actor)
            .is_some_and(|released| released.same_channel(&self.release.subscribe()))
        {
            held.remove(&self.actor);
        }
        drop(held);
        self.release.send_replace(true);
    }
}

/// The checkpoint-write collectors observing an engine's session commits.
#[derive(Default)]
pub(crate) struct CommitObservers {
    collectors: Mutex<Vec<CheckpointWriteCollector>>,
}

impl CommitObservers {
    /// Record every later session commit into `collector` too.
    pub(crate) fn observe(&self, collector: CheckpointWriteCollector) {
        self.collectors.lock_recover().push(collector);
    }

    fn current(&self) -> Vec<CheckpointWriteCollector> {
        self.collectors.lock_recover().clone()
    }
}

/// A store set whose durable port waits out held session actors and
/// observes session commits.
pub(crate) struct HoldingStoreSet {
    inner: Arc<lash_sqlite_store::SqliteStoreSet>,
    holds: Arc<SessionHolds>,
    observers: Arc<CommitObservers>,
}

impl HoldingStoreSet {
    pub(crate) fn new(
        inner: Arc<lash_sqlite_store::SqliteStoreSet>,
        holds: Arc<SessionHolds>,
        observers: Arc<CommitObservers>,
    ) -> Self {
        Self {
            inner,
            holds,
            observers,
        }
    }
}

impl lash_core_execution::StoreSet for HoldingStoreSet {
    fn durable_store(&self) -> Arc<dyn DurableStore> {
        Arc::new(HoldingDurableStore {
            inner: lash_core_execution::StoreSet::durable_store(self.inner.as_ref()),
            holds: Arc::clone(&self.holds),
            observers: Arc::clone(&self.observers),
            sessions: lash_core_execution::StoreSet::session_store_factory(self.inner.as_ref()),
        })
    }

    fn node_wakes(&self) -> Option<Arc<dyn lash_durable::NodeWakes>> {
        lash_core_execution::StoreSet::node_wakes(self.inner.as_ref())
    }

    fn binding_identity(&self) -> &lash_core_execution::StoreBindingId {
        lash_core_execution::StoreSet::binding_identity(self.inner.as_ref())
    }

    fn clock(&self) -> Arc<dyn lash_core_execution::Clock> {
        lash_core_execution::StoreSet::clock(self.inner.as_ref())
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core_execution::DeploymentStore> {
        lash_core_execution::StoreSet::session_store_factory(self.inner.as_ref())
    }

    fn attachment_referrers(&self) -> Arc<dyn lash_core_execution::AttachmentReferrers> {
        lash_core_execution::StoreSet::attachment_referrers(self.inner.as_ref())
    }

    fn process_registry(&self) -> Arc<dyn lash_core_execution::ProcessRegistry> {
        lash_core_execution::StoreSet::process_registry(self.inner.as_ref())
    }

    fn trigger_store(&self) -> Arc<dyn lash_core_execution::TriggerStore> {
        lash_core_execution::StoreSet::trigger_store(self.inner.as_ref())
    }

    fn process_env_store(&self) -> Arc<dyn lash_core_execution::ProcessExecutionEnvStore> {
        lash_core_execution::StoreSet::process_env_store(self.inner.as_ref())
    }

    fn turn_prelude_store(&self) -> Arc<dyn lash_core_execution::TurnPreludeStore> {
        lash_core_execution::StoreSet::turn_prelude_store(self.inner.as_ref())
    }

    fn tool_material_store(&self) -> Arc<dyn lash_core_execution::store::ToolMaterialStore> {
        lash_core_execution::StoreSet::tool_material_store(self.inner.as_ref())
    }

    fn definition_store(&self) -> Arc<dyn lash_core_execution::ProcessDefinitionStore> {
        lash_core_execution::StoreSet::definition_store(self.inner.as_ref())
    }

    fn attachment_store(&self) -> Arc<dyn lash_core_execution::AttachmentStore> {
        lash_core_execution::StoreSet::attachment_store(self.inner.as_ref())
    }

    fn module_artifacts(&self) -> Arc<dyn lash_core_execution::ModuleArtifactStore> {
        lash_core_execution::StoreSet::module_artifacts(self.inner.as_ref())
    }

    fn recovery_leader(&self) -> Arc<dyn lash_core_execution::store::RecoveryLeaderStore> {
        lash_core_execution::StoreSet::recovery_leader(self.inner.as_ref())
    }

    fn obligation_ledger(
        &self,
        kind: lash_core_execution::store::ObligationKind,
    ) -> Arc<dyn lash_core_execution::store::ObligationLedger> {
        lash_core_execution::StoreSet::obligation_ledger(self.inner.as_ref(), kind)
    }

    fn artifact_cleanup(&self) -> Arc<dyn lash_core_execution::store::ArtifactCleanupLedger> {
        lash_core_execution::StoreSet::artifact_cleanup(self.inner.as_ref())
    }
}

/// The engine's durable port with held session actors waited out and
/// session commits observed.
struct HoldingDurableStore {
    inner: Arc<dyn DurableStore>,
    holds: Arc<SessionHolds>,
    observers: Arc<CommitObservers>,
    /// The session store the observed commits are read back from.
    sessions: Arc<dyn lash_core_execution::DeploymentStore>,
}

#[async_trait::async_trait]
impl DurableStore for HoldingDurableStore {
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

    async fn live_decodes(&self) -> Result<Vec<Vec<lash_durable::FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    /// The one held call: an owner's transaction over a held actor opens
    /// only once the hold is released.
    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.holds.pass(actor).await;
        self.inner.begin(actor, epoch).await
    }

    /// Observed once it commits: each session commit the transaction
    /// carried is recorded into every installed collector. A commit that
    /// cannot be observed fails loudly as a corrupt store, so a run never
    /// passes on evidence it lost.
    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let collectors = self.observers.current();
        let session_commits = if collectors.is_empty() {
            Vec::new()
        } else {
            tx.domain()
                .iter()
                .filter_map(|write| match write {
                    DomainWrite::SessionCommit(commit) => Some(commit.commit_json.clone()),
                    _ => None,
                })
                .collect()
        };
        let committed = self.inner.commit(tx, label).await?;
        let unobserved = |error: lash_core::store::StoreError| {
            DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Corrupt,
                message: format!("the simulator could not observe a session commit: {error}"),
            })
        };
        for commit_json in session_commits {
            let commit =
                lash_core_store::store::decode_session_commit(&commit_json).map_err(unobserved)?;
            for collector in &collectors {
                collector
                    .observe_committed(self.sessions.as_ref(), &commit)
                    .await
                    .map_err(unobserved)?;
            }
        }
        Ok(committed)
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.inner.commit_mail(tx, label).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }
}

#[async_trait::async_trait]
impl DurableReads for HoldingDurableStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        self.inner.run_records(owner).await
    }

    async fn run_record_owners(
        &self,
        actor: &lash_durable::ActorKey,
    ) -> Result<Vec<lash_durable::domain::OwnerKey>, DurableError> {
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
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
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
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}
