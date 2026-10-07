//! [`SimNodes`]: several production runners over one database.
//!
//! Each node is one boot of a production [`Runner`] with its own
//! [`FaultStore`] and its own view of the shared [`SimClock`]. Killing a node
//! drops every task and byte it held in memory; the database keeps
//! everything it committed. Starting it again registers a new boot.

use crate::clock::{SimClock, settle};
use crate::fault::{Activity, FaultStore};
use crate::life::{Life, NodeClock, NodeLife};
use crate::script::Script;
use lash_durable::runner::{Activation, Drain, Hints, Runner, RunnerConfig, Stopped};
use lash_durable::{
    CommitLabel, DurableError, DurableStore, FormatSet, LeaseConfig, MailCommit, MailTx, NodeId,
};
use lash_sansio::sync::MutexExt as _;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

/// How every node of a simulated deployment runs.
#[derive(Clone, Debug)]
pub struct SimNodesConfig {
    pub lease: LeaseConfig,
    pub decodes: Vec<FormatSet>,
    pub max_active: usize,
}

struct SimNode {
    life: Arc<NodeLife>,
    store: Arc<FaultStore>,
    hints: Hints,
    drain: Drain,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: JoinHandle<Option<Result<Stopped, DurableError>>>,
}

/// Several runner instances over one database, on one virtual clock, under
/// one fault script.
pub struct SimNodes {
    database: Arc<dyn DurableStore>,
    clock: Arc<SimClock>,
    script: Script,
    activity: Arc<Activity>,
    config: SimNodesConfig,
    activation: Arc<dyn Activation>,
    nodes: Arc<Mutex<BTreeMap<String, SimNode>>>,
    /// The format sets a node decodes when it is not the config's: a node
    /// of another build.
    decodes: Mutex<BTreeMap<String, Vec<FormatSet>>>,
}

impl SimNodes {
    /// A deployment over `database`, whose clock must be `clock`, running
    /// `activation` for every claimed actor. It starts with no node.
    pub fn new(
        database: Arc<dyn DurableStore>,
        clock: Arc<SimClock>,
        script: Script,
        config: SimNodesConfig,
        activation: Arc<dyn Activation>,
    ) -> Self {
        Self {
            database,
            clock,
            script,
            activity: Arc::default(),
            config,
            activation,
            nodes: Arc::default(),
            decodes: Mutex::default(),
        }
    }

    /// Run every later boot of `node` as a build that decodes `decodes`.
    pub fn decode_on(&self, node: &str, decodes: Vec<FormatSet>) {
        self.decodes
            .lock_recover()
            .insert(node.to_string(), decodes);
    }

    /// Start draining `node`'s live boot by release.
    pub fn drain(&self, node: &str) {
        if let Some(sim) = self.nodes.lock_recover().get(node) {
            sim.drain.start();
        }
    }

    /// The database, unfaulted: for laws' reads.
    pub fn database(&self) -> &Arc<dyn DurableStore> {
        &self.database
    }

    pub fn clock(&self) -> &Arc<SimClock> {
        &self.clock
    }

    pub fn script(&self) -> &Script {
        &self.script
    }

    /// Start a new boot of `node`, killing a live one first.
    pub fn start(&self, node: &str) {
        self.kill(node);
        let name: Arc<str> = Arc::from(node);
        let life = NodeLife::new();
        let store = Arc::new(FaultStore::new(
            Arc::clone(&self.database),
            Arc::clone(&name),
            Arc::clone(&self.script.shared),
            Arc::clone(&life),
            Arc::clone(&self.clock),
            Arc::clone(&self.activity),
        ));
        let decodes = self
            .decodes
            .lock_recover()
            .get(node)
            .cloned()
            .unwrap_or_else(|| self.config.decodes.clone());
        let drain = Drain::default();
        let runner = Runner::new(
            Arc::clone(&store) as Arc<dyn DurableStore>,
            NodeClock::new(Arc::clone(&self.clock), Arc::clone(&life)),
            RunnerConfig {
                node: NodeId::new(node),
                decodes,
                lease: self.config.lease,
                max_active: self.config.max_active,
                claim_batch: self.config.max_active,
            },
            Arc::clone(&self.activation),
        )
        .with_drain(drain.clone());
        let hints = runner.hints();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task_life = Arc::clone(&life);
        let task = tokio::spawn(async move {
            tokio::select! {
                stopped = runner.run(async {
                    let _ = stopped.await;
                }) => Some(stopped),
                () = task_life.dead() => None,
            }
        });
        self.nodes.lock_recover().insert(
            node.to_string(),
            SimNode {
                life,
                store,
                hints,
                drain,
                stop: Some(stop),
                task,
            },
        );
    }

    /// Kill `node`: its runner and every activation it ran are dropped
    /// mid-await, and none of its calls enters the store again.
    pub fn kill(&self, node: &str) {
        if let Some(sim) = self.nodes.lock_recover().get(node) {
            sim.life.kill();
            sim.task.abort();
        }
    }

    /// Kill `node` and start a new boot of it: its in-memory state is gone,
    /// the database keeps what it committed.
    pub fn restart(&self, node: &str) {
        self.start(node);
    }

    /// Pause `node` whole: its store calls and timers hold until it resumes.
    pub fn pause(&self, node: &str) {
        if let Some(sim) = self.nodes.lock_recover().get(node) {
            sim.life.pause();
        }
    }

    pub fn resume(&self, node: &str) {
        if let Some(sim) = self.nodes.lock_recover().get(node) {
            sim.life.resume();
        }
    }

    /// Ask `node` to stop cleanly: it releases its actors.
    pub fn stop(&self, node: &str) {
        if let Some(sim) = self.nodes.lock_recover().get_mut(node)
            && let Some(stop) = sim.stop.take()
        {
            let _ = stop.send(());
        }
    }

    /// `node`'s life: dead once it, or a fault, killed it.
    pub fn life(&self, node: &str) -> Life {
        self.nodes
            .lock_recover()
            .get(node)
            .map_or(Life::Dead, |sim| sim.life.get())
    }

    /// Whether `node`'s runner still serves: alive, and not stopped.
    pub fn serving(&self, node: &str) -> bool {
        self.nodes
            .lock_recover()
            .get(node)
            .is_some_and(|sim| sim.life.get() != Life::Dead && !sim.task.is_finished())
    }

    /// Every node that is paused now.
    pub fn paused(&self) -> Vec<String> {
        self.nodes
            .lock_recover()
            .iter()
            .filter(|(_, sim)| sim.life.get() == Life::Paused)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Why `node`'s runner stopped, once it has: `None` while it serves or
    /// when it was killed.
    pub async fn stopped(&self, node: &str) -> Option<Result<Stopped, DurableError>> {
        let task = {
            let mut nodes = self.nodes.lock_recover();
            let sim = nodes.get_mut(node)?;
            if !sim.task.is_finished() {
                return None;
            }
            std::mem::replace(&mut sim.task, tokio::spawn(async { None }))
        };
        task.await.ok().flatten()
    }

    /// `node`'s store, for a producer on that node.
    pub fn store(&self, node: &str) -> Option<Arc<dyn DurableStore>> {
        self.nodes
            .lock_recover()
            .get(node)
            .map(|sim| Arc::clone(&sim.store) as Arc<dyn DurableStore>)
    }

    /// Commit `tx` from `node` under `label` and deliver its wakes: to the
    /// woken actor's owner when it is owned, else to every node's claim
    /// loop. A wake a fault lost reaches nobody.
    pub async fn mail(
        &self,
        node: &str,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let store = self
            .store(node)
            .ok_or_else(|| DurableError::NodeLeaseLost {
                node: NodeId::new(node),
            })?;
        let commit = store.commit_mail(tx, label).await?;
        deliver(&self.nodes, &commit);
        Ok(commit)
    }

    /// Cut `node` off from its lease: every heartbeat it sends fails before
    /// it enters the store, while its bodies and every other call carry on,
    /// so past its lease it is a zombie whose commits a fence must refuse.
    pub fn partition(&self, node: &str) {
        if let Some(sim) = self.nodes.lock_recover().get(node) {
            sim.life.partition(true);
        }
    }

    /// Reconnect a partitioned `node` to its lease.
    pub fn heal(&self, node: &str) {
        if let Some(sim) = self.nodes.lock_recover().get(node) {
            sim.life.partition(false);
        }
    }

    /// A producer's store: the database as a host outside every node writes
    /// it (an admission, a cancel request, a resolve), labelled and cut
    /// under the script as `name`'s writes. It runs no runner and owns no
    /// actor; a fault that kills it parks every later call it makes.
    ///
    /// Its wakes reach the woken actor's owner when it is owned, and
    /// otherwise the first running node by name only, as one notification
    /// reaches one listener first: the other nodes find the actor at their
    /// next poll, so which node claims it stays a function of the run.
    pub fn producer(&self, name: &str) -> Arc<dyn DurableStore> {
        let nodes = Arc::clone(&self.nodes);
        Arc::new(
            FaultStore::new(
                Arc::clone(&self.database),
                Arc::from(name),
                Arc::clone(&self.script.shared),
                NodeLife::new(),
                Arc::clone(&self.clock),
                Arc::clone(&self.activity),
            )
            .with_wakes(Arc::new(move |commit: &MailCommit| {
                deliver_first(&nodes, commit);
            })),
        )
    }

    /// Wait until no node waits on the database and every task woken so far
    /// has reached its next timer or gate.
    pub async fn quiesce(&self) {
        loop {
            self.activity.idle().await;
            let entered = self.activity.entered();
            settle().await;
            if self.activity.entered() == entered {
                self.activity.idle().await;
                if self.activity.entered() == entered {
                    return;
                }
            }
        }
    }

    /// Quiesce, then move time to the next armed timer and quiesce again.
    /// Answers the new time, or `None` when nothing is armed.
    pub async fn step(&self) -> Option<u64> {
        self.quiesce().await;
        let due = self.clock.advance_to_next_due().await;
        self.quiesce().await;
        due
    }
}

/// Deliver `commit`'s wakes: to the woken actor's owner when it is owned,
/// else to every node's claim loop. A wake a fault lost reaches nobody.
fn deliver(nodes: &Mutex<BTreeMap<String, SimNode>>, commit: &MailCommit) {
    let nodes = nodes.lock_recover();
    for woken in &commit.woken {
        match &woken.owner {
            Some(owner) => {
                if let Some(sim) = nodes.get(owner.node.as_str()) {
                    sim.hints.wake(woken);
                }
            }
            None => {
                for sim in nodes.values() {
                    sim.hints.wake(woken);
                }
            }
        }
    }
}

/// Deliver `commit`'s wakes to the woken actor's owner when it is owned,
/// else to the first running node by name.
fn deliver_first(nodes: &Mutex<BTreeMap<String, SimNode>>, commit: &MailCommit) {
    let nodes = nodes.lock_recover();
    for woken in &commit.woken {
        let target = match &woken.owner {
            Some(owner) => nodes.get(owner.node.as_str()),
            None => nodes
                .values()
                .find(|sim| sim.life.get() == Life::Running && !sim.task.is_finished()),
        };
        if let Some(sim) = target {
            sim.hints.wake(woken);
        }
    }
}

impl Drop for SimNodes {
    fn drop(&mut self) {
        for sim in self.nodes.lock_recover().values() {
            sim.life.kill();
            sim.task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::Fault;
    use crate::testing::{FORMATS, actor, sqlite};
    use lash_durable::runner::Owned;
    use lash_durable::{Epoch, MailKind, MailSeq};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const ACK: CommitLabel = CommitLabel::new("t.ack");

    /// Holds every claimed actor hot forever, acknowledging its mail.
    #[derive(Default)]
    struct Hold {
        live: AtomicUsize,
        acked: Mutex<Vec<(Epoch, MailSeq)>>,
    }

    struct Live<'a>(&'a AtomicUsize);

    impl Drop for Live<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl Activation for Hold {
        async fn activate(&self, owned: Owned) {
            self.live.fetch_add(1, Ordering::SeqCst);
            let _live = Live(&self.live);
            loop {
                let Ok(mut tx) = owned.begin().await else {
                    return;
                };
                let seqs: Vec<MailSeq> = tx.mail().iter().map(|mail| mail.seq).collect();
                if !seqs.is_empty() {
                    tx.ack_seen();
                    if owned.commit(tx, ACK).await.is_ok() {
                        let mut acked = self.acked.lock_recover();
                        acked.extend(seqs.into_iter().map(|seq| (owned.epoch(), seq)));
                    }
                }
                owned.wait_for_mail().await;
            }
        }
    }

    async fn step_until(nodes: &SimNodes, reached: impl Fn() -> bool) {
        for _ in 0..200 {
            if reached() {
                return;
            }
            nodes.step().await;
        }
        panic!("not reached; {}", nodes.script().rendered_trace());
    }

    /// A deployment of `hold` on one node, `a`, under `script`, holding
    /// actor `one` hot.
    async fn holding(script: Script, hold: &Arc<Hold>) -> SimNodes {
        let clock = SimClock::new();
        let nodes = SimNodes::new(
            sqlite(Arc::clone(&clock)).await,
            clock,
            script,
            SimNodesConfig {
                lease: LeaseConfig::default(),
                decodes: vec![FormatSet::new(FORMATS)],
                max_active: 4,
            },
            Arc::clone(hold) as Arc<dyn Activation>,
        );
        nodes.start("a");
        let mut create = MailTx::new();
        create
            .create_actor(actor("one"), FormatSet::new(FORMATS))
            .append(actor("one"), MailKind::new("t"), "first");
        nodes
            .mail("a", create, CommitLabel::new("t.create"))
            .await
            .unwrap();
        step_until(&nodes, || hold.acked.lock_recover().len() == 1).await;
        nodes
    }

    /// The first heartbeat answers a minute after it commits: a heartbeat
    /// that hangs.
    fn hung_heartbeat() -> Script {
        let script = Script::new();
        script.cut_on(
            "a",
            CommitLabel::HEARTBEAT,
            1,
            Fault::DelayedAck(Duration::from_secs(60)),
        );
        script
    }

    /// A node whose heartbeat hangs stops itself at `self_stop_after` past
    /// its last renewal (its registration here), and its activations stop
    /// with it, though the heartbeat has not returned (FIG-5178).
    #[tokio::test]
    async fn a_hung_heartbeat_still_self_stops_at_self_stop_after() {
        let hold = Arc::new(Hold::default());
        let nodes = holding(hung_heartbeat(), &hold).await;
        let self_stop_after = u64::try_from(
            LeaseConfig::default()
                .settings()
                .self_stop_after
                .as_millis(),
        )
        .unwrap();
        while nodes.serving("a") && nodes.clock().logical_ms() <= 3 * self_stop_after {
            if nodes.step().await.is_none() {
                break;
            }
        }
        let at = nodes.clock().logical_ms();
        assert!(
            !nodes.serving("a"),
            "still serving {at} ms after its last renewal"
        );
        assert!(
            at <= self_stop_after,
            "stopped {at} ms after its last renewal, past self_stop_after ({self_stop_after} ms)"
        );
        assert_eq!(nodes.stopped("a").await, Some(Ok(Stopped::Unrenewed)));
        assert_eq!(
            hold.live.load(Ordering::SeqCst),
            0,
            "its activation stopped with it"
        );
    }

    /// A node asked to stop while its heartbeat hangs stops at once: the
    /// stop is heard during the heartbeat, not after it (FIG-5178).
    #[tokio::test]
    async fn a_node_asked_to_stop_while_its_heartbeat_hangs_stops_at_once() {
        let hold = Arc::new(Hold::default());
        let nodes = holding(hung_heartbeat(), &hold).await;
        let heartbeat_every = u64::try_from(
            LeaseConfig::default()
                .settings()
                .heartbeat_every
                .as_millis(),
        )
        .unwrap();
        while nodes.clock().logical_ms() <= heartbeat_every {
            nodes.step().await;
        }
        let asked = nodes.clock().logical_ms();
        nodes.stop("a");
        for _ in 0..8 {
            if !nodes.serving("a") {
                break;
            }
            nodes.quiesce().await;
        }
        assert_eq!(
            nodes.clock().logical_ms(),
            asked,
            "time moved before it stopped"
        );
        assert_eq!(nodes.stopped("a").await, Some(Ok(Stopped::Requested)));
        assert_eq!(hold.live.load(Ordering::SeqCst), 0);
    }

    /// A restarted node keeps nothing it held in memory: its old boot's
    /// activations are dropped, the new boot takes the actor under a bumped
    /// epoch, and only what the database committed survives, so no
    /// acknowledged mail is delivered again.
    #[tokio::test]
    async fn a_restarted_node_keeps_only_what_the_database_has() {
        let hold = Arc::new(Hold::default());
        let nodes = holding(Script::new(), &hold).await;
        let before = nodes
            .database()
            .actor(&actor("one"))
            .await
            .unwrap()
            .unwrap();

        nodes.restart("a");
        let mut mail = MailTx::new();
        mail.append(actor("one"), MailKind::new("t"), "second");
        nodes
            .mail("a", mail, CommitLabel::new("t.mail"))
            .await
            .unwrap();
        step_until(&nodes, || hold.acked.lock_recover().len() == 2).await;

        let after = nodes
            .database()
            .actor(&actor("one"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            after.epoch > before.epoch,
            "the new boot owns under a bumped epoch"
        );
        assert_ne!(
            after.owner.map(|owner| owner.boot),
            before.owner.map(|owner| owner.boot)
        );
        assert_eq!(
            hold.live.load(Ordering::SeqCst),
            1,
            "the old boot's activation is gone"
        );
        let acked = hold.acked.lock_recover().clone();
        assert_ne!(
            acked[0].1, acked[1].1,
            "acknowledged mail is never delivered again"
        );
        assert!(acked[1].0 > acked[0].0);
    }
}
