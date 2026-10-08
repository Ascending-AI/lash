//! [`FaultStore`]: one node's view of the deployment's database, cut at its
//! labelled writes.
//!
//! Every write a node makes goes through its fault store: the store numbers
//! it under its label, records it, and applies the [`Fault`] a rule gives
//! it. An owner commit's domain rows travel inside its transaction, so a cut
//! at a label cuts them with it. Reads (`now`, `begin`, `actor`, `owned` and
//! the domain [`DurableReads`]) pass through, but a paused or dead node makes
//! none, as a stopped process could not; a read rule fails one `actor` read.

use crate::clock::{SimClock, settle};
use crate::life::NodeLife;
use crate::script::{Entry, Fault, Shared, Stored, WriteKind};
use lash_durable::domain::{
    AdmittedId, ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordKind,
    RunRecordRow, RunRecordWrite, ScopeKey, SessionCloseRow, SnapshotRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DomainWrite, DurableError,
    DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx,
    NodeLease, NodeSpec, Reaped, StoreFailure, StoreFailureKind,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ProcessId, SessionId};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A contested write waiting for its turn: its node, then its arrival.
type Ticket = (Arc<str>, u64);

/// The next change of a waiting node's life.
type LifeChange = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Writes in flight across a deployment: a simulation moves its clock only
/// when no node waits on the database.
///
/// It also orders the runners' contested writes. Nodes that claim or reap
/// at one virtual instant race for the same rows, and which one the
/// database answered first used to turn on wall-clock time: on the store's
/// worker thread, and on how far each node's earlier calls had got. So
/// each claim and reap first waits its [turn](Self::turn), and enters the
/// store only once no call is in flight, nothing holds the clock and a
/// settle starts neither, lowest node name first, one at a time. Which node claims is a function of the
/// scenario, never of timing (FIG-5284).
#[derive(Debug, Default)]
pub(crate) struct Activity {
    in_flight: AtomicUsize,
    entered: AtomicUsize,
    /// The contested writes waiting for their turn, with the life of the
    /// node that waits.
    turns: Mutex<BTreeMap<Ticket, Arc<NodeLife>>>,
    arrivals: AtomicU64,
    /// Rings when a call enters or leaves and when a turn comes or goes.
    changed: tokio::sync::Notify,
}

impl Activity {
    fn enter(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    fn leave(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    /// How many store calls and turns ever entered.
    pub(crate) fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    /// Wait until no store call is in flight and no live node waits for
    /// its turn.
    pub(crate) async fn idle(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let lives = self.lives();
            if self.in_flight.load(Ordering::SeqCst) == 0 && self.first_live().is_none() {
                return;
            }
            either(changed, lives).await;
        }
    }

    /// One store call, counted while it runs.
    async fn call<T>(&self, call: impl Future<Output = T>) -> T {
        struct Leave<'a>(&'a Activity);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.0.leave();
            }
        }
        self.enter();
        let _leave = Leave(self);
        call.await
    }

    /// Wait for `node`'s turn at a contested write: until no call is in
    /// flight and nothing holds `clock`, a settle later still, and this is
    /// the first turn of a live node by node name. A paused node, or one
    /// whose runner is held, keeps its place but holds no other node back;
    /// a killed one gives it up.
    async fn turn(&self, node: &Arc<str>, life: &Arc<NodeLife>, clock: &SimClock) {
        struct Waiting<'a> {
            activity: &'a Activity,
            ticket: Ticket,
        }
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.activity.turns.lock_recover().remove(&self.ticket);
                self.activity.changed.notify_waiters();
            }
        }
        let ticket = (
            Arc::clone(node),
            self.arrivals.fetch_add(1, Ordering::SeqCst),
        );
        self.turns
            .lock_recover()
            .insert(ticket.clone(), Arc::clone(life));
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        let waiting = Waiting {
            activity: self,
            ticket,
        };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let lives = self.lives();
            if self.is_turn(&waiting.ticket) {
                clock.unheld().await;
                let entered = self.entered() + clock.holds_taken();
                settle().await;
                clock.unheld().await;
                if self.entered() + clock.holds_taken() == entered
                    && self.take_turn(&waiting.ticket)
                {
                    return;
                }
                continue;
            }
            either(changed, lives).await;
        }
    }

    /// The first waiting turn of a live node.
    fn first_live(&self) -> Option<Ticket> {
        self.turns
            .lock_recover()
            .iter()
            .find(|(_, life)| life.live())
            .map(|(ticket, _)| ticket.clone())
    }

    fn is_turn(&self, ticket: &Ticket) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0 && self.first_live().as_ref() == Some(ticket)
    }

    /// Take `ticket`'s turn if it is still its turn.
    fn take_turn(&self, ticket: &Ticket) -> bool {
        let mut turns = self.turns.lock_recover();
        let first = turns
            .iter()
            .find(|(_, life)| life.live())
            .map(|(first, _)| first);
        if self.in_flight.load(Ordering::SeqCst) != 0 || first != Some(ticket) {
            return false;
        }
        turns.remove(ticket);
        true
    }

    /// The next change of life of every node waiting for its turn.
    fn lives(&self) -> Vec<LifeChange> {
        self.turns
            .lock_recover()
            .values()
            .map(|life| Box::pin(life.changes()) as LifeChange)
            .collect()
    }
}

/// Wait until `changed` rings or one of `lives` changes.
async fn either(
    mut changed: Pin<&mut tokio::sync::futures::Notified<'_>>,
    mut lives: Vec<LifeChange>,
) {
    std::future::poll_fn(|cx| {
        if changed.as_mut().poll(cx).is_ready()
            || lives
                .iter_mut()
                .any(|life| life.as_mut().poll(cx).is_ready())
        {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

/// One write, owning its arguments, ready to send.
type Call<T> = Pin<Box<dyn Future<Output = Result<T, DurableError>> + Send>>;

/// What a store does with a mailbox commit's wakes once it committed.
pub(crate) type Wakes = Arc<dyn Fn(&MailCommit) + Send + Sync>;

/// One node's store: the deployment's database under the deployment's
/// fault script.
pub struct FaultStore {
    inner: Arc<dyn DurableStore>,
    node: Arc<str>,
    script: Arc<Shared>,
    life: Arc<NodeLife>,
    clock: Arc<SimClock>,
    activity: Arc<Activity>,
    wakes: Option<Wakes>,
}

/// How a write's result reads as a stored outcome.
trait Effect {
    fn effective(&self) -> bool {
        true
    }
}

impl Effect for NodeLease {}
impl Effect for HeartbeatOutcome {}
impl Effect for ActorCommit {}

impl Effect for MailCommit {}
impl Effect for () {}

impl Effect for Vec<Reaped> {
    fn effective(&self) -> bool {
        !self.is_empty()
    }
}

impl Effect for Vec<Claimed> {
    fn effective(&self) -> bool {
        !self.is_empty()
    }
}

impl Effect for Vec<ActorKey> {}

impl FaultStore {
    pub(crate) fn new(
        inner: Arc<dyn DurableStore>,
        node: Arc<str>,
        script: Arc<Shared>,
        life: Arc<NodeLife>,
        clock: Arc<SimClock>,
        activity: Arc<Activity>,
    ) -> Self {
        Self {
            inner,
            node,
            script,
            life,
            clock,
            activity,
            wakes: None,
        }
    }

    /// Hand every committed mailbox write's wakes to `wakes`, as a
    /// producer's backend hints the nodes it woke.
    pub(crate) fn with_wakes(mut self, wakes: Wakes) -> Self {
        self.wakes = Some(wakes);
        self
    }

    /// One read: a paused or dead node makes none.
    async fn read<T>(&self, read: impl Future<Output = T>) -> T {
        self.life.running().await;
        if self.life.activations_first() {
            tokio::task::yield_now().await;
        }
        self.activity.call(read).await
    }

    /// One labelled write under the script.
    async fn write<T: Effect + Send + 'static>(
        &self,
        kind: WriteKind,
        label: CommitLabel,
        actor: Option<&ActorKey>,
        write: Call<T>,
        lose_wake: impl FnOnce(&mut T),
    ) -> Result<T, DurableError> {
        self.write_carrying(kind, label, actor, Vec::new(), write, lose_wake)
            .await
    }

    /// [`Self::write`], recording the `x_start` records it appends.
    async fn write_carrying<T: Effect + Send + 'static>(
        &self,
        kind: WriteKind,
        label: CommitLabel,
        actor: Option<&ActorKey>,
        starts: Vec<AdmittedId>,
        write: Call<T>,
        lose_wake: impl FnOnce(&mut T),
    ) -> Result<T, DurableError> {
        self.life.running().await;
        let entry = self
            .script
            .enter(&self.node, kind, label, actor, self.clock.logical_ms());
        entry.carrying(starts);
        let Some(fault) = entry.fault else {
            return self.store(entry, write).await;
        };
        match fault {
            Fault::FailBefore => {
                entry.finish(Stored::NotEntered);
                Err(DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable,
                    message: format!("injected: {label} failed before it entered the store"),
                }))
            }
            Fault::Abort => {
                entry.finish(Stored::NotEntered);
                self.life.kill();
                self.life.running().await;
                unreachable!("a dead node's call never returns")
            }
            Fault::CommitThenAbort => {
                let _ = self.store(entry, write).await;
                self.life.kill();
                self.life.running().await;
                unreachable!("a dead node's call never returns")
            }
            Fault::AckHidden => {
                let _ = self.store(entry, write).await;
                Err(DurableError::AckLost { label })
            }
            Fault::DelayedAck(delay) => {
                let answer = self.store(entry, write).await;
                lash_core_ids::clock::Clock::sleep(&*self.clock, delay).await;
                answer
            }
            Fault::StaleEpoch => {
                let answer = self.store(entry, write).await;
                self.life.pause();
                self.life.running().await;
                answer
            }
            Fault::Zombie => {
                // The write is already on its way: it enters the store when
                // the node resumes, even if the resumed node stops itself
                // and drops the call that sent it.
                self.life.pause();
                let life = Arc::clone(&self.life);
                self.send(async move { life.running().await }, entry, write)
                    .await
            }
            Fault::LostWake => {
                let mut answer = self.store(entry, write).await;
                if let Ok(answer) = &mut answer {
                    lose_wake(answer);
                }
                answer
            }
        }
    }

    /// Send one write to the store. It runs on a task of its own, as a
    /// transaction runs in the database: once sent it commits or is refused
    /// even if the node that sent it dies meanwhile, and the trace records
    /// which.
    async fn store<T: Effect + Send + 'static>(
        &self,
        entry: Entry,
        write: Call<T>,
    ) -> Result<T, DurableError> {
        self.send(std::future::ready(()), entry, write).await
    }

    /// [`Self::store`], with the write entering the store only once `held`
    /// is done.
    async fn send<T: Effect + Send + 'static>(
        &self,
        held: impl Future<Output = ()> + Send + 'static,
        entry: Entry,
        write: Call<T>,
    ) -> Result<T, DurableError> {
        let activity = Arc::clone(&self.activity);
        let sent = tokio::spawn(async move {
            held.await;
            let answer = activity.call(write).await;
            entry.finish(match &answer {
                Ok(answer) => Stored::Committed {
                    effective: answer.effective(),
                },
                Err(error) => Stored::Refused(error.clone()),
            });
            answer
        });
        match sent.await {
            Ok(answer) => answer,
            Err(failed) if failed.is_panic() => std::panic::resume_unwind(failed.into_panic()),
            Err(_) => std::future::pending().await,
        }
    }

    /// A write on the store, owning what it needs.
    fn call<T, F>(&self, write: impl FnOnce(Arc<dyn DurableStore>) -> F) -> Call<T>
    where
        F: Future<Output = Result<T, DurableError>> + Send + 'static,
    {
        Box::pin(write(Arc::clone(&self.inner)))
    }
}

fn keep<T>(_: &mut T) {}

#[async_trait::async_trait]
impl DurableStore for FaultStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.read(self.inner.now()).await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.write(
            WriteKind::Lease,
            CommitLabel::NODE_REGISTER,
            None,
            self.call(|store| {
                let spec = spec.clone();
                async move { store.register_node(&spec).await }
            }),
            keep,
        )
        .await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        if self.life.partitioned() {
            self.life.running().await;
            let entry = self.script.enter(
                &self.node,
                WriteKind::Lease,
                CommitLabel::HEARTBEAT,
                None,
                self.clock.logical_ms(),
            );
            entry.finish(Stored::NotEntered);
            return Err(DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Unavailable,
                message: "injected: the node is partitioned from its lease".to_owned(),
            }));
        }
        self.write(
            WriteKind::Lease,
            CommitLabel::HEARTBEAT,
            None,
            self.call(|store| {
                let node = node.clone();
                async move { store.heartbeat(&node).await }
            }),
            keep,
        )
        .await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.activity
            .turn(&self.node, &self.life, &self.clock)
            .await;
        self.write(
            WriteKind::Lease,
            CommitLabel::REAP,
            None,
            self.call(|store| {
                let reaper = reaper.clone();
                async move { store.reap(&reaper).await }
            }),
            keep,
        )
        .await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.write(
            WriteKind::Lease,
            CommitLabel::NODE_RELEASE,
            None,
            self.call(|store| {
                let node = node.clone();
                async move { store.release_node(&node).await }
            }),
            keep,
        )
        .await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.activity
            .turn(&self.node, &self.life, &self.clock)
            .await;
        self.write(
            WriteKind::Lease,
            CommitLabel::CLAIM,
            None,
            self.call(|store| {
                let node = node.clone();
                async move { store.claim(&node, limit).await }
            }),
            keep,
        )
        .await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.write(
            WriteKind::Lease,
            CommitLabel::NODE_DRAIN,
            None,
            self.call(|store| {
                let node = node.clone();
                async move { store.mark_draining(&node).await }
            }),
            keep,
        )
        .await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<lash_durable::FormatSet>>, DurableError> {
        self.read(self.inner.live_decodes()).await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.read(self.inner.owned(node)).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.read(self.inner.begin(actor, epoch)).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let actor = tx.actor().clone();
        self.script.observe(label, tx.domain());
        let refusal = self.script.refusal(label, tx.domain());
        let starts = tx
            .domain()
            .iter()
            .filter_map(|write| match write {
                DomainWrite::RunRecord(RunRecordWrite::Append {
                    owner,
                    run,
                    ordinal,
                    kind: RunRecordKind::XStart,
                    ..
                }) => Some(AdmittedId {
                    owner: owner.clone(),
                    run: *run,
                    ordinal: *ordinal,
                }),
                _ => None,
            })
            .collect();
        self.write_carrying(
            WriteKind::Actor,
            label,
            Some(&actor),
            starts,
            self.call(|store| async move {
                if let Some(refusal) = refusal {
                    Err(DurableError::Domain(refusal))
                } else {
                    store.commit(tx, label).await
                }
            }),
            keep,
        )
        .await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let commit = self
            .write(
                WriteKind::Mail,
                label,
                None,
                self.call(|store| async move { store.commit_mail(tx, label).await }),
                |commit: &mut MailCommit| commit.woken.clear(),
            )
            .await?;
        if let Some(wakes) = &self.wakes {
            wakes(&commit);
        }
        Ok(commit)
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        if self.script.fails_read(actor) {
            return self
                .read(async {
                    Err(DurableError::Store(StoreFailure {
                        kind: StoreFailureKind::Unavailable,
                        message: format!("injected: the read of {actor} failed"),
                    }))
                })
                .await;
        }
        self.read(self.inner.actor(actor)).await
    }
}

#[async_trait::async_trait]
impl DurableReads for FaultStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.read(self.inner.turn(session)).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        self.read(self.inner.turn_namespaces(session, run)).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        self.read(self.inner.turn_end(session, run)).await
    }

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        self.read(self.inner.run_records(owner)).await
    }

    async fn run_record_owners(
        &self,
        actor: &lash_durable::ActorKey,
    ) -> Result<Vec<lash_durable::domain::OwnerKey>, DurableError> {
        self.read(self.inner.run_record_owners(actor)).await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        self.read(self.inner.snapshot(exec)).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.read(self.inner.pending_waits(owner)).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.read(self.inner.wait(id)).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
        self.read(self.inner.process(process)).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.read(self.inner.live_until_descendants(scope, limit))
            .await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.read(self.inner.until_children(scope, after, limit))
            .await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.read(self.inner.session_close(session)).await
    }

    async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
        self.read(self.inner.ending_scopes(session)).await
    }

    async fn session_mailbox(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
        self.read(self.inner.session_mailbox(session)).await
    }

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.read(self.inner.park_events(after, limit)).await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        self.read(self.inner.prompt_snapshot(call)).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        self.read(self.inner.prompt_texts(hashes)).await
    }
}

#[cfg(test)]
#[path = "fault_domain_tests.rs"]
mod domain_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::settle;
    use crate::life::Life;
    use crate::script::Script;
    use crate::testing::{FORMATS, actor, sqlite};
    use lash_durable::{MailKind, MailTx};
    use std::time::Duration;

    const CREATE: CommitLabel = CommitLabel::new("t.create");
    const MAIL: CommitLabel = CommitLabel::new("t.mail");
    const ACK: CommitLabel = CommitLabel::new("t.ack");

    struct Deployment {
        clock: Arc<SimClock>,
        database: Arc<dyn DurableStore>,
        script: Script,
        activity: Arc<Activity>,
    }

    impl Deployment {
        async fn new() -> Self {
            let clock = SimClock::new();
            Self {
                database: sqlite(Arc::clone(&clock)).await,
                clock,
                script: Script::new(),
                activity: Arc::default(),
            }
        }

        fn node(&self, name: &str) -> (Arc<FaultStore>, Arc<NodeLife>) {
            let life = NodeLife::new();
            let store = Arc::new(FaultStore::new(
                Arc::clone(&self.database),
                Arc::from(name),
                Arc::clone(&self.script.shared),
                Arc::clone(&life),
                Arc::clone(&self.clock),
                Arc::clone(&self.activity),
            ));
            (store, life)
        }

        fn spec(name: &str) -> NodeSpec {
            NodeSpec {
                node: lash_durable::NodeId::new(name),
                decodes: vec![lash_durable::FormatSet::new(FORMATS)],
                ttl_millis: 15_000,
            }
        }
    }

    fn create(id: &str) -> MailTx {
        let mut tx = MailTx::new();
        tx.create_actor(actor(id), lash_durable::FormatSet::new(FORMATS));
        tx
    }

    fn mail(id: &str) -> MailTx {
        let mut tx = MailTx::new();
        tx.append(actor(id), MailKind::new("t"), "body");
        tx
    }

    /// A node's actor `id` with one mail, claimed by `store` under `lease`.
    async fn owned_with_mail(store: &FaultStore, lease: &NodeLease, id: &str) -> Epoch {
        store.commit_mail(create(id), CREATE).await.expect("create");
        store.commit_mail(mail(id), MAIL).await.expect("mail");
        let claimed = store.claim(lease, 1).await.expect("claim");
        assert_eq!(claimed.len(), 1, "the new actor is claimable");
        claimed[0].epoch
    }

    /// Fail-before answers a store failure and abort kills the node; under
    /// both, nothing the write carried reaches the store.
    #[tokio::test]
    async fn fail_before_and_abort_keep_the_write_out_of_the_store() {
        let deployment = Deployment::new().await;
        let (store, life) = deployment.node("a");
        deployment
            .script
            .cut_on("a", CREATE, 1, Fault::FailBefore)
            .cut_on("a", CREATE, 2, Fault::Abort);

        let refused = store.commit_mail(create("one"), CREATE).await;
        assert!(matches!(
            refused,
            Err(DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Unavailable,
                ..
            }))
        ));
        assert_eq!(life.get(), Life::Running);

        let call = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.commit_mail(create("one"), CREATE).await }
        });
        settle().await;
        assert!(!call.is_finished(), "a dead node's call never returns");
        assert_eq!(life.get(), Life::Dead);
        call.abort();

        assert_eq!(
            deployment.database.actor(&actor("one")).await.unwrap(),
            None
        );
        let stored: Vec<Stored> = deployment
            .script
            .trace()
            .into_iter()
            .map(|write| write.stored)
            .collect();
        assert_eq!(stored, vec![Stored::NotEntered, Stored::NotEntered]);
    }

    /// Commit-then-abort and ack-hidden both commit the write; the first
    /// kills the node before the reply, the second answers `AckLost`.
    #[tokio::test]
    async fn commit_then_abort_and_ack_hidden_commit_but_withhold_the_reply() {
        let deployment = Deployment::new().await;
        let (a, a_life) = deployment.node("a");
        let (b, b_life) = deployment.node("b");
        deployment
            .script
            .cut_on("a", CREATE, 1, Fault::CommitThenAbort)
            .cut_on("b", CREATE, 1, Fault::AckHidden);

        let call = tokio::spawn({
            let a = Arc::clone(&a);
            async move { a.commit_mail(create("one"), CREATE).await }
        });
        deployment.script.settled(1).await;
        settle().await;
        assert!(!call.is_finished(), "a dead node's call never returns");
        assert_eq!(a_life.get(), Life::Dead);
        call.abort();

        assert_eq!(
            b.commit_mail(create("two"), CREATE).await,
            Err(DurableError::AckLost { label: CREATE })
        );
        assert_eq!(b_life.get(), Life::Running);

        for id in ["one", "two"] {
            assert!(
                deployment
                    .database
                    .actor(&actor(id))
                    .await
                    .unwrap()
                    .is_some(),
                "{id} was committed"
            );
        }
    }

    /// A delayed ack commits at once and answers after exactly its delay of
    /// virtual time.
    #[tokio::test]
    async fn a_delayed_ack_answers_after_its_virtual_delay() {
        let deployment = Deployment::new().await;
        let (store, _life) = deployment.node("a");
        deployment
            .script
            .cut_on("a", CREATE, 1, Fault::DelayedAck(Duration::from_secs(2)));

        let call = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.commit_mail(create("one"), CREATE).await }
        });
        deployment.clock.wait_for_sleep(2_000).await;
        assert!(
            deployment
                .database
                .actor(&actor("one"))
                .await
                .unwrap()
                .is_some()
        );
        deployment.clock.advance_to(1_999).await;
        settle().await;
        assert!(!call.is_finished());
        deployment.clock.advance_to(2_000).await;
        call.await.expect("the call ran").expect("the commit stood");
    }

    /// A zombie's write is held, with its node, before it enters the store;
    /// resumed after its node was reaped and its actor claimed elsewhere,
    /// it is refused for ownership and leaves no trace in the actor.
    #[tokio::test]
    async fn a_zombie_write_enters_only_on_resume_and_is_fenced() {
        let deployment = Deployment::new().await;
        let (a, a_life) = deployment.node("a");
        let (b, _) = deployment.node("b");
        let a_lease = a.register_node(&Deployment::spec("a")).await.unwrap();
        let epoch = owned_with_mail(&a, &a_lease, "one").await;
        let before = deployment
            .database
            .actor(&actor("one"))
            .await
            .unwrap()
            .unwrap();
        deployment.script.cut_on("a", ACK, 1, Fault::Zombie);

        let mut tx = a.begin(&actor("one"), epoch).await.unwrap();
        tx.ack_seen();
        let call = tokio::spawn({
            let a = Arc::clone(&a);
            async move { a.commit(tx, ACK).await }
        });
        settle().await;
        assert_eq!(a_life.get(), Life::Paused);
        let zombie = deployment.script.trace().last().cloned().unwrap();
        assert_eq!(
            zombie.stored,
            Stored::Pending,
            "the paused write has not entered"
        );

        deployment.clock.advance_by(16_000).await;
        let b_lease = b.register_node(&Deployment::spec("b")).await.unwrap();
        let reaped = b.reap(&b_lease).await.unwrap();
        assert!(reaped.iter().any(|reaped| reaped.actor == actor("one")));
        let claimed = b.claim(&b_lease, 1).await.unwrap();
        assert!(claimed[0].epoch > epoch);

        a_life.resume();
        let refused = call.await.expect("the call ran");
        assert!(matches!(refused, Err(DurableError::OwnershipLost(_))));
        let after = deployment
            .database
            .actor(&actor("one"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.revision, before.revision,
            "nothing the zombie wrote is visible"
        );
        assert_eq!(after.pending_mail, before.pending_mail);
        let zombie = deployment
            .script
            .trace()
            .into_iter()
            .find(|write| write.cut == Some(Fault::Zombie))
            .unwrap();
        assert!(matches!(
            zombie.stored,
            Stored::Refused(DurableError::OwnershipLost(_))
        ));
    }

    /// A stale-epoch cut commits the write, then pauses the node with the
    /// reply until the node resumes.
    #[tokio::test]
    async fn a_stale_epoch_cut_commits_then_holds_the_reply_with_its_node() {
        let deployment = Deployment::new().await;
        let (a, a_life) = deployment.node("a");
        let lease = a.register_node(&Deployment::spec("a")).await.unwrap();
        let epoch = owned_with_mail(&a, &lease, "one").await;
        deployment.script.cut_on("a", ACK, 1, Fault::StaleEpoch);

        let mut tx = a.begin(&actor("one"), epoch).await.unwrap();
        tx.ack_seen();
        let settled = deployment.script.trace().len();
        let call = tokio::spawn({
            let a = Arc::clone(&a);
            async move { a.commit(tx, ACK).await }
        });
        deployment.script.settled(settled + 1).await;
        settle().await;
        assert_eq!(a_life.get(), Life::Paused);
        assert!(!call.is_finished(), "the reply waits for the node");
        let committed = deployment
            .database
            .actor(&actor("one"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            committed.pending_mail, 0,
            "the write committed before the pause"
        );

        a_life.resume();
        call.await.expect("the call ran").expect("the commit stood");
    }

    /// A lost wake commits the mail and answers no woken actor, so no hint
    /// can be delivered; the mail itself stands.
    #[tokio::test]
    async fn a_lost_wake_commits_the_mail_but_announces_no_wake() {
        let deployment = Deployment::new().await;
        let (store, _life) = deployment.node("a");
        deployment.script.cut_on("a", MAIL, 2, Fault::LostWake);
        store.commit_mail(create("one"), CREATE).await.unwrap();

        let announced = store.commit_mail(mail("one"), MAIL).await.unwrap();
        assert!(!announced.woken.is_empty());
        let lost = store.commit_mail(mail("one"), MAIL).await.unwrap();
        assert!(lost.woken.is_empty());
        assert_eq!(lost.appended.len(), 1);
        let snapshot = deployment
            .database
            .actor(&actor("one"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.pending_mail, 2);
    }
}
