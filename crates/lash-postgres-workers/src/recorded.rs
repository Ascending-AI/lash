//! The node's view of its own durable writes, and the partition fault.
//!
//! [`RecordedStores`] wraps the PostgreSQL store set so the backend's
//! durable store and signals are [`RecordedStore`] and [`RecordedSignals`]:
//! each forwards every call unchanged and reports the answer of every lease
//! call, claim, reap and owner commit ([`Event`]). The node's runner, the
//! activations and the fences are the production ones; the decorators only
//! watch.
//!
//! The one fault they inject is the partition: while the test holds the
//! node's heartbeat ([`RecordedStore::block_heartbeat`]), every heartbeat
//! waits, and everything else, the bodies and their owner commits among it,
//! goes on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core_execution::{
    AttachmentStore, Clock, DeploymentStore, ModuleArtifactStore, ProcessExecutionEnvStore,
    ProcessRegistry, StoreBindingId, StoreSet, TriggerStore,
};
use lash_durable::domain::{
    ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow, ScopeKey,
    SessionCloseRow, SnapshotRow, TurnEnd, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, BootLiveness, Claimed, CommitLabel,
    DurableError, DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit,
    MailTx, NodeLease, NodeSpec, Owner, Reaped, SignalFeed, Signals, WakeBatch,
};
use lash_sansio::{ProcessId, SessionId, TurnId};
use tokio::sync::Notify;

use crate::events::{Event, report};

/// The heartbeat gate the test controls.
#[derive(Debug, Default)]
struct Gate {
    blocked: AtomicBool,
    opened: Notify,
}

impl Gate {
    async fn pass(&self) {
        loop {
            let opened = self.opened.notified();
            if !self.blocked.load(Ordering::SeqCst) {
                return;
            }
            opened.await;
        }
    }
}

/// A durable store that reports what it answered.
pub struct RecordedStore {
    inner: Arc<dyn DurableStore>,
    node: String,
    gate: Gate,
}

impl RecordedStore {
    /// Report `inner`'s answers as `node`'s.
    #[must_use]
    pub fn new(inner: Arc<dyn DurableStore>, node: &str) -> Self {
        Self {
            inner,
            node: node.to_owned(),
            gate: Gate::default(),
        }
    }

    /// Hold every heartbeat from now on, or let them through again.
    pub fn block_heartbeat(&self, blocked: bool) {
        self.gate.blocked.store(blocked, Ordering::SeqCst);
        if !blocked {
            self.gate.opened.notify_waiters();
        }
        report(&self.node, Event::HeartbeatBlocked { blocked });
    }

    fn reaped(&self, reaped: &[Reaped], by: &str) {
        for reaped in reaped {
            report(
                &self.node,
                Event::Reaped {
                    actor: reaped.actor.to_string(),
                    from: reaped.from.node.to_string(),
                    epoch: reaped.epoch.0,
                    by: by.to_owned(),
                },
            );
        }
    }

    fn claimed(&self, claimed: &[Claimed]) {
        for claimed in claimed {
            report(
                &self.node,
                Event::Claimed {
                    actor: claimed.actor.to_string(),
                    epoch: claimed.epoch.0,
                },
            );
        }
    }
}

#[async_trait::async_trait]
impl DurableStore for RecordedStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        let lease = self.inner.register_node(spec).await;
        if let Ok(lease) = &lease {
            report(
                &self.node,
                Event::Registered {
                    boot: lease.owner.boot.to_string(),
                },
            );
        }
        lease
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.gate.pass().await;
        let outcome = self.inner.heartbeat(node).await;
        let said = match &outcome {
            Ok(HeartbeatOutcome::Renewed { .. }) => "renewed".to_owned(),
            Ok(HeartbeatOutcome::Reaped) => "reaped".to_owned(),
            Err(error) => error.to_string(),
        };
        report(&self.node, Event::Heartbeat { outcome: said });
        outcome
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        let reaped = self.inner.reap(reaper).await;
        if let Ok(reaped) = &reaped {
            self.reaped(reaped, "lease");
        }
        reaped
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        let released = self.inner.release_node(node).await;
        if let Ok(actors) = &released {
            report(
                &self.node,
                Event::Released {
                    actors: actors.iter().map(ToString::to_string).collect(),
                },
            );
        }
        released
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        let claimed = self.inner.claim(node, limit).await;
        if let Ok(claimed) = &claimed {
            self.claimed(claimed);
        }
        claimed
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<lash_durable::FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        let owned = self.inner.owned(node).await;
        if let Ok(owned) = &owned {
            self.claimed(owned);
        }
        owned
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        let opened = self.inner.begin(actor, epoch).await;
        if let Err(error) = &opened {
            let outcome = match error {
                DurableError::OwnershipLost(_) => "ownership_lost",
                _ => "failed",
            };
            report(
                &self.node,
                Event::BeginRefused {
                    actor: actor.to_string(),
                    epoch: epoch.0,
                    outcome: outcome.to_owned(),
                    error: error.to_string(),
                },
            );
        }
        opened
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let actor = tx.actor().to_string();
        let epoch = tx.epoch().0;
        let committed = self.inner.commit(tx, label).await;
        let (outcome, error) = match &committed {
            Ok(_) => ("committed", None),
            Err(DurableError::OwnershipLost(fenced)) => {
                ("ownership_lost", Some(format!("{fenced:?}")))
            }
            Err(error) => ("failed", Some(error.to_string())),
        };
        report(
            &self.node,
            Event::Commit {
                actor,
                epoch,
                label: label.as_str().to_owned(),
                outcome: outcome.to_owned(),
                error,
            },
        );
        committed
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
impl DurableReads for RecordedStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
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
}

/// Signals that report the reaps their liveness locks found, and each
/// change in what their probe saw.
pub struct RecordedSignals {
    inner: Arc<dyn Signals>,
    store: Arc<RecordedStore>,
    seen: std::sync::Mutex<Option<(Vec<String>, Vec<String>)>>,
}

#[async_trait::async_trait]
impl Signals for RecordedSignals {
    async fn publish(&self, batch: &WakeBatch) -> Result<(), DurableError> {
        self.inner.publish(batch).await
    }

    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn SignalFeed>, DurableError> {
        self.inner.listen(lease).await
    }

    async fn liveness(&self) -> Result<Vec<BootLiveness>, DurableError> {
        let probed = self.inner.liveness().await;
        if let Ok(boots) = &probed {
            let mut held = Vec::new();
            let mut free = Vec::new();
            for boot in boots
                .iter()
                .filter(|boot| boot.boot.node.as_str() != self.store.node)
            {
                let name = boot.boot.node.to_string();
                if boot.held {
                    held.push(name)
                } else {
                    free.push(name)
                }
            }
            let now = Some((held.clone(), free.clone()));
            let mut seen = self
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *seen != now {
                *seen = now;
                report(&self.store.node, Event::Liveness { held, free });
            }
        }
        probed
    }

    async fn reap_released(
        &self,
        reaper: &NodeLease,
        boot: &Owner,
    ) -> Result<Vec<Reaped>, DurableError> {
        let reaped = self.inner.reap_released(reaper, boot).await;
        report(
            &self.store.node,
            Event::ReapAttempt {
                of: boot.node.to_string(),
                outcome: match &reaped {
                    Ok(reaped) => reaped.len().to_string(),
                    Err(error) => error.to_string(),
                },
            },
        );
        if let Ok(reaped) = &reaped {
            self.store.reaped(reaped, "lock");
        }
        reaped
    }
}

/// A store set whose durable store and signals report what they answered;
/// every other port is the inner set's.
pub struct RecordedStores {
    inner: Arc<dyn StoreSet>,
    store: Arc<RecordedStore>,
}

impl RecordedStores {
    /// `inner`, with its durable store and signals reported as `node`'s.
    #[must_use]
    pub fn new(inner: Arc<dyn StoreSet>, node: &str) -> Self {
        let store = Arc::new(RecordedStore::new(inner.durable_store(), node));
        Self { inner, store }
    }

    /// The recorded durable store, for the partition switch.
    #[must_use]
    pub fn store(&self) -> Arc<RecordedStore> {
        Arc::clone(&self.store)
    }
}

impl StoreSet for RecordedStores {
    fn durable_store(&self) -> Arc<dyn DurableStore> {
        Arc::clone(&self.store) as _
    }

    fn durable_signals(&self) -> Option<Arc<dyn Signals>> {
        self.inner.durable_signals().map(|inner| {
            Arc::new(RecordedSignals {
                inner,
                store: Arc::clone(&self.store),
                seen: std::sync::Mutex::default(),
            }) as _
        })
    }

    fn binding_identity(&self) -> &StoreBindingId {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.inner.session_store_factory()
    }

    fn attachment_referrers(&self) -> Arc<dyn lash_core_execution::store::AttachmentReferrers> {
        self.inner.attachment_referrers()
    }

    fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        self.inner.process_registry()
    }

    fn trigger_store(&self) -> Arc<dyn TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn turn_prelude_store(&self) -> Arc<dyn lash_core_execution::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }

    fn tool_material_store(&self) -> Arc<dyn lash_core_execution::store::ToolMaterialStore> {
        self.inner.tool_material_store()
    }

    fn definition_store(&self) -> Arc<dyn lash_core_execution::ProcessDefinitionStore> {
        self.inner.definition_store()
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.inner.module_artifacts()
    }

    fn recovery_leader(&self) -> Arc<dyn lash_core_execution::store::RecoveryLeaderStore> {
        self.inner.recovery_leader()
    }

    fn obligation_ledger(
        &self,
        kind: lash_core_execution::store::ObligationKind,
    ) -> Arc<dyn lash_core_execution::store::ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }

    fn artifact_cleanup(&self) -> Arc<dyn lash_core_execution::store::ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }
}
