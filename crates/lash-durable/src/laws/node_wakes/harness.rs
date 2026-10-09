//! What the node-wake laws run nodes with: production runners holding every
//! actor they claim hot, and a store that counts each node's claim attempts.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, mpsc};

use super::super::{LawBroken, LawResult};
use crate::domain;
use crate::runner::{Activation, Exit, Hints, Owned, Runner, RunnerConfig, Stopped};
use crate::{
    ActorKey, ActorSnapshot, ActorTx, BootLiveness, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableSettings, DurableStore, Epoch, FormatSet,
    HeartbeatOutcome, LeaseSettings, MailCommit, MailKind, MailTx, NodeId, NodeLease, NodeSpec,
    NodeWakes, Owner, Reaped, WakeBatch,
};
use lash_sansio::{ProcessId, SessionId, TurnId};

/// The format set every law actor and node uses.
pub(super) fn formats() -> FormatSet {
    FormatSet::new("law-formats")
}

pub(super) fn actor(name: &str) -> Result<ActorKey, LawBroken> {
    ActorKey::session(name).map_err(|error| LawBroken(error.to_string()))
}

/// Register node `name` on `store` with the default fifteen-second lease.
pub(super) async fn node(store: &dyn DurableStore, name: &str) -> Result<NodeLease, LawBroken> {
    Ok(store
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![formats()],
            ttl_millis: 15_000,
        })
        .await?)
}

/// Create `actor` ready and unowned, answering what the commit woke.
pub(super) async fn create(
    store: &dyn DurableStore,
    actor: &ActorKey,
) -> Result<MailCommit, LawBroken> {
    let mut tx = MailTx::new();
    tx.create_actor(actor.clone(), formats());
    Ok(store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await?)
}

/// One note for `actor`.
pub(super) fn mail(actor: &ActorKey) -> MailTx {
    let mut tx = MailTx::new();
    tx.append(actor.clone(), MailKind::new("law.note"), "note");
    tx
}

/// The node that owns `actor` now, if any.
pub(super) async fn owner_of(
    store: &dyn DurableStore,
    actor: &ActorKey,
) -> Result<Option<String>, LawBroken> {
    Ok(store
        .actor(actor)
        .await?
        .and_then(|snapshot| snapshot.owner)
        .map(|owner| owner.node.as_str().to_owned()))
}

/// Every boot's liveness lock, as a probe sees it now.
pub(super) async fn liveness(node_wakes: &dyn NodeWakes) -> Result<Vec<BootLiveness>, LawBroken> {
    Ok(node_wakes.liveness().await?)
}

/// Wait on stored state, yielding between reads. The test action's watchdog
/// bounds hangs; scheduler latency is never part of a law's verdict.
pub(super) async fn eventually<F, Fut>(mut reached: F) -> LawResult
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool, LawBroken>>,
{
    while !reached().await? {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

/// Wait until `node`'s listener holds its liveness lock.
pub(super) async fn listening(node_wakes: &dyn NodeWakes, node: &str) -> LawResult {
    eventually(|| async {
        Ok(liveness(node_wakes)
            .await?
            .iter()
            .any(|liveness| liveness.boot.node.as_str() == node && liveness.held))
    })
    .await
}

/// Holds every actor it claims hot, acknowledging its mail and reporting
/// when each arrived.
struct Hold {
    arrived: mpsc::UnboundedSender<()>,
    waiting: mpsc::UnboundedSender<()>,
}

#[async_trait::async_trait]
impl Activation for Hold {
    async fn activate(&self, owned: Owned) -> Exit {
        loop {
            let Ok(mut tx) = owned.begin().await else {
                return Exit::Released;
            };
            if !tx.mail().is_empty() {
                tx.ack_seen();
                if owned.commit(tx, CommitLabel::new("law.ack")).await.is_err() {
                    return Exit::Released;
                }
                let _ = self.arrived.send(());
            }
            let _ = self.waiting.send(());
            owned.wait_for_mail().await;
        }
    }
}

/// A production runner serving one law node, which holds what it claims.
/// Dropping it stops the node as a crash would: its task is aborted.
pub struct LawNode {
    /// The hints the node's mailbox writers hand what their commits woke.
    pub hints: Hints,
    arrived: mpsc::UnboundedReceiver<()>,
    waiting: mpsc::UnboundedReceiver<()>,
    task: tokio::task::JoinHandle<Result<Stopped, DurableError>>,
}

impl LawNode {
    /// Wait until an activation has read its mailbox and is waiting for
    /// a hint or its mail poll. A later append cannot race its initial read.
    pub(super) async fn waiting(&mut self) -> LawResult {
        self.waiting
            .recv()
            .await
            .ok_or_else(|| LawBroken("the owner stopped before waiting for mail".to_owned()))
    }

    /// Wait for the next mail to reach one of the node's actors.
    pub(super) async fn arrived(&mut self) -> LawResult {
        self.arrived
            .recv()
            .await
            .ok_or_else(|| LawBroken("the node stopped running its actors".to_owned()))
    }

    /// Whether the node stopped serving.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Kill the node, as its process dying would, and wait until it is gone.
    pub async fn kill(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for LawNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Serve node `name` over `store` under `settings`, holding what it claims,
/// with `node_wakes` when given: production's runner on the system clock.
///
/// # Panics
///
/// When `settings` do not validate: the law's own settings are wrong.
#[must_use]
#[expect(clippy::expect_used, reason = "a law's own settings validate")]
pub fn serve_node(
    store: Arc<dyn DurableStore>,
    node_wakes: Option<Arc<dyn NodeWakes>>,
    name: &str,
    settings: DurableSettings,
) -> LawNode {
    let config = settings.validate().expect("the law's settings validate");
    let (send, arrived) = mpsc::unbounded_channel();
    let (idle, waiting) = mpsc::unbounded_channel();
    let mut runner = Runner::new(
        store,
        Arc::new(lash_core_ids::clock::SystemClock),
        RunnerConfig::new(NodeId::new(name), vec![formats()], &config),
        Arc::new(Hold {
            arrived: send,
            waiting: idle,
        }),
    );
    if let Some(node_wakes) = node_wakes {
        runner = runner.with_node_wakes(node_wakes);
    }
    let hints = runner.hints();
    let task = tokio::spawn(runner.run(std::future::pending()));
    LawNode {
        hints,
        arrived,
        waiting,
        task,
    }
}

/// `lease`'s settings, otherwise the defaults.
pub(super) fn under(lease: LeaseSettings) -> DurableSettings {
    DurableSettings {
        lease,
        ..DurableSettings::default()
    }
}

/// Settings that run at most `max_active` actors under `lease`.
pub(super) fn holding(lease: LeaseSettings, max_active: usize) -> DurableSettings {
    let defaults = DurableSettings::default();
    DurableSettings {
        lease,
        max_active,
        claim_batch: defaults.claim_batch.min(max_active),
        ..defaults
    }
}

/// Put fallback polls effectively out of reach of a test action. Both the
/// floor and ceiling must be disabled: a successful claim resets the floor.
pub(super) fn quiet() -> LeaseSettings {
    LeaseSettings {
        claim_poll: Duration::from_secs(100 * 365 * 24 * 60 * 60),
        claim_backoff: Duration::from_secs(100 * 365 * 24 * 60 * 60),
        ..LeaseSettings::default()
    }
}

/// Every claim call the counted nodes made, as (node, actors taken).
#[derive(Clone, Default)]
pub(super) struct Claims {
    attempts: Arc<std::sync::Mutex<Vec<(String, usize)>>>,
    changed: Arc<Notify>,
}

impl Claims {
    /// Await completed claim calls, rather than racing a wall-clock bound.
    pub(super) async fn wait_for(&self, count: usize) {
        loop {
            let changed = self.changed.notified();
            if self.len() >= count {
                return;
            }
            changed.await;
        }
    }

    pub(super) fn len(&self) -> usize {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub(super) fn take(&self) -> Vec<(String, usize)> {
        std::mem::take(
            &mut *self
                .attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// The seam the wake laws count claim attempts through: a durable store
/// that records each claim call and forwards every call unchanged.
pub(super) struct CountingStore {
    pub(super) inner: Arc<dyn DurableStore>,
    pub(super) node: String,
    pub(super) claims: Claims,
}

#[async_trait::async_trait]
impl DurableStore for CountingStore {
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
        let claimed = self.inner.claim(node, limit).await;
        if let Ok(claimed) = &claimed {
            self.claims
                .attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((self.node.clone(), claimed.len()));
            self.claims.changed.notify_one();
        }
        claimed
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

    async fn commit(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<crate::ActorCommit, DurableError> {
        self.inner.commit(tx, label).await
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
impl DurableReads for CountingStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<domain::TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<domain::TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<domain::TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_records(
        &self,
        owner: &domain::OwnerKey,
    ) -> Result<Vec<domain::RunRecordRow>, DurableError> {
        self.inner.run_records(owner).await
    }

    async fn snapshot(
        &self,
        exec: &domain::ExecKey,
    ) -> Result<Option<domain::SnapshotRow>, DurableError> {
        self.inner.snapshot(exec).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<domain::WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &domain::WaitId) -> Result<Option<domain::WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(
        &self,
        process: &ProcessId,
    ) -> Result<Option<domain::ProcessActorRow>, DurableError> {
        self.inner.process(process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &domain::ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.live_until_descendants(scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &domain::ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.until_children(scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<domain::SessionCloseRow>, DurableError> {
        self.inner.session_close(session).await
    }

    async fn ending_scopes(
        &self,
        session: &SessionId,
    ) -> Result<Vec<domain::ScopeKey>, DurableError> {
        self.inner.ending_scopes(session).await
    }

    async fn session_mailbox(
        &self,
        session: &SessionId,
    ) -> Result<domain::SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
    }

    async fn park_events(
        &self,
        after: Option<domain::ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<domain::ParkEventRow>, DurableError> {
        self.inner.park_events(after, limit).await
    }

    async fn prompt_snapshot(
        &self,
        call: &domain::PromptCallKey,
    ) -> Result<Option<domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}

/// Evidence from calls made by a production runner, forwarded unchanged.
pub(super) enum WakeEvidence {
    Published(WakeBatch),
    Watched(Vec<BootLiveness>),
    Reaped(Vec<Reaped>),
}

pub(super) struct Evidence(mpsc::UnboundedReceiver<WakeEvidence>);

impl Evidence {
    pub(super) async fn until(
        &mut self,
        matches: impl Fn(&WakeEvidence) -> bool,
    ) -> Result<WakeEvidence, LawBroken> {
        while let Some(event) = self.0.recv().await {
            if matches(&event) {
                return Ok(event);
            }
        }
        Err(LawBroken(
            "the node stopped before reporting its wake evidence".to_owned(),
        ))
    }
}

struct ObservedWakes {
    inner: Arc<dyn NodeWakes>,
    evidence: mpsc::UnboundedSender<WakeEvidence>,
}

pub(super) fn observing(inner: Arc<dyn NodeWakes>) -> (Arc<dyn NodeWakes>, Evidence) {
    let (evidence, received) = mpsc::unbounded_channel();
    (
        Arc::new(ObservedWakes { inner, evidence }),
        Evidence(received),
    )
}

#[async_trait::async_trait]
impl NodeWakes for ObservedWakes {
    async fn publish(&self, batch: &WakeBatch) -> Result<(), DurableError> {
        self.inner.publish(batch).await?;
        let _ = self.evidence.send(WakeEvidence::Published(batch.clone()));
        Ok(())
    }

    async fn listen(
        &self,
        lease: &NodeLease,
    ) -> Result<Box<dyn crate::NodeWakeFeed>, DurableError> {
        self.inner.listen(lease).await
    }

    async fn liveness(&self) -> Result<Vec<BootLiveness>, DurableError> {
        let boots = self.inner.liveness().await?;
        let _ = self.evidence.send(WakeEvidence::Watched(boots.clone()));
        Ok(boots)
    }

    async fn reap_released(
        &self,
        reaper: &NodeLease,
        boot: &Owner,
    ) -> Result<Vec<Reaped>, DurableError> {
        let reaped = self.inner.reap_released(reaper, boot).await?;
        if !reaped.is_empty() {
            let _ = self.evidence.send(WakeEvidence::Reaped(reaped.clone()));
        }
        Ok(reaped)
    }
}
