//! [`FaultStore`]: one node's view of the deployment's database, cut at its
//! labelled writes.
//!
//! Every write a node makes goes through its fault store: the store numbers
//! it under its label, records it, and applies the [`Fault`] a rule gives
//! it. Reads (`now`, `begin`, `actor`, `owned`) pass through, but a paused
//! or dead node makes none, as a stopped process could not.

use crate::clock::SimClock;
use crate::life::NodeLife;
use crate::script::{Entry, Fault, Shared, Stored, WriteKind};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx, NodeLease, NodeSpec,
    Reaped, StoreFailure, StoreFailureKind,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Writes in flight across a deployment: a simulation moves its clock only
/// when no node waits on the database.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    in_flight: AtomicUsize,
    entered: AtomicUsize,
    idle: tokio::sync::Notify,
}

impl Activity {
    fn enter(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.in_flight.fetch_add(1, Ordering::SeqCst);
    }

    fn leave(&self) {
        if self.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }

    /// How many store calls ever entered.
    pub(crate) fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    /// Wait until no store call is in flight.
    pub(crate) async fn idle(&self) {
        loop {
            let idle = self.idle.notified();
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            idle.await;
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
}

/// One write, owning its arguments, ready to send.
type Call<T> = Pin<Box<dyn Future<Output = Result<T, DurableError>> + Send>>;

/// One node's store: the deployment's database under the deployment's
/// fault script.
pub struct FaultStore {
    inner: Arc<dyn DurableStore>,
    node: Arc<str>,
    script: Arc<Shared>,
    life: Arc<NodeLife>,
    clock: Arc<SimClock>,
    activity: Arc<Activity>,
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
        }
    }

    /// One read: a paused or dead node makes none.
    async fn read<T>(&self, read: impl Future<Output = T>) -> T {
        self.life.running().await;
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
        self.life.running().await;
        let entry = self
            .script
            .enter(&self.node, kind, label, actor, self.clock.logical_ms());
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
                self.life.pause();
                self.life.running().await;
                self.store(entry, write).await
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
        let activity = Arc::clone(&self.activity);
        let sent = tokio::spawn(async move {
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

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.read(self.inner.owned(node)).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.read(self.inner.begin(actor, epoch)).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let actor = tx.actor().clone();
        self.write(
            WriteKind::Actor,
            label,
            Some(&actor),
            self.call(|store| async move { store.commit(tx, label).await }),
            keep,
        )
        .await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.write(
            WriteKind::Mail,
            label,
            None,
            self.call(|store| async move { store.commit_mail(tx, label).await }),
            |commit: &mut MailCommit| commit.woken.clear(),
        )
        .await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.read(self.inner.actor(actor)).await
    }
}

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
