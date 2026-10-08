//! Laws of the durability engine's cross-node signals over PostgreSQL (L8,
//! FIG-5178), on real connections and the database clock.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_durable::runner::{Activation, Exit, Owned, Runner, RunnerConfig, Stopped};
use lash_durable::{
    ActorState, CommitLabel, DurableError, DurableSettings, DurableStore, FormatSet,
    HeartbeatOutcome, LeaseSettings, MailKind, MailTx, NodeSpec, Release, domain,
};
use lash_sansio::{ProcessId, SessionId, TurnId};
use tokio::sync::mpsc;

use super::*;
use crate::PostgresStorage;
use crate::testing::IsolatedDatabase;

const TTL: Duration = Duration::from_secs(15);

fn formats() -> FormatSet {
    FormatSet::new("law-formats")
}

fn actor(name: &str) -> ActorKey {
    ActorKey::session(name).expect("a law's actor key")
}

async fn database(law: &str) -> Option<IsolatedDatabase> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping {law}: database URL is not set");
        return None;
    };
    Some(IsolatedDatabase::create(&database_url).await)
}

async fn storage(database: &IsolatedDatabase) -> PostgresStorage {
    crate::testing::connect(database.url())
        .await
        .expect("open the isolated store")
}

async fn node(store: &dyn DurableStore, name: &str) -> NodeLease {
    store
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![formats()],
            ttl_millis: i64::try_from(TTL.as_millis()).expect("ttl fits"),
        })
        .await
        .expect("register a node")
}

async fn create(store: &dyn DurableStore, actor: &ActorKey) {
    let mut tx = MailTx::new();
    tx.create_actor(actor.clone(), formats());
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await
        .expect("create the actor");
}

fn mail(actor: &ActorKey) -> MailTx {
    let mut tx = MailTx::new();
    tx.append(actor.clone(), MailKind::new("law.note"), "note");
    tx
}

/// Whether `boot`'s liveness lock is held, as a probe sees it now.
async fn held(signals: &PostgresSignals, boot: &Owner) -> Option<bool> {
    signals
        .liveness()
        .await
        .expect("probe liveness")
        .into_iter()
        .find(|liveness| liveness.boot == *boot)
        .map(|liveness| liveness.held)
}

/// Poll `reached` every 25 ms until it holds or `within` passes; answers how
/// long it took.
async fn eventually<F, Fut>(within: Duration, what: &str, mut reached: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = Instant::now();
    while !reached().await {
        assert!(started.elapsed() < within, "{what} not within {within:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    started.elapsed()
}

/// Holds every actor it claims hot, acknowledging its mail and reporting
/// when each arrived.
struct Hold {
    arrived: mpsc::UnboundedSender<Instant>,
}

#[async_trait::async_trait]
impl Activation for Hold {
    async fn activate(&self, owned: Owned) -> Exit {
        loop {
            let Ok(mut tx) = owned.begin().await else {
                return Exit::Released;
            };
            if !tx.mail().is_empty() {
                let arrived = Instant::now();
                tx.ack_seen();
                if owned.commit(tx, CommitLabel::new("law.ack")).await.is_err() {
                    return Exit::Released;
                }
                let _ = self.arrived.send(arrived);
            }
            owned.wait_for_mail().await;
        }
    }
}

/// A runner over `storage` with signals, holding what it claims.
struct Node {
    hints: lash_durable::runner::Hints,
    arrived: mpsc::UnboundedReceiver<Instant>,
    task: tokio::task::JoinHandle<Result<Stopped, DurableError>>,
}

fn start(storage: &PostgresStorage, name: &str, lease: LeaseSettings, signals: bool) -> Node {
    let signals = signals.then(|| Arc::new(storage.durable_signals()) as Arc<dyn Signals>);
    let settings = DurableSettings {
        lease,
        ..DurableSettings::default()
    };
    run(Arc::new(storage.durable_store()), signals, name, settings)
}

/// A runner with signals over `storage` under `settings`, whose every claim
/// `claims` counts.
fn start_counted(
    storage: &PostgresStorage,
    name: &str,
    settings: DurableSettings,
    claims: &Claims,
) -> Node {
    let store = CountingStore {
        inner: storage.durable_store(),
        node: name.to_owned(),
        claims: claims.clone(),
    };
    let signals: Arc<dyn Signals> = Arc::new(storage.durable_signals());
    run(Arc::new(store), Some(signals), name, settings)
}

fn run(
    store: Arc<dyn DurableStore>,
    signals: Option<Arc<dyn Signals>>,
    name: &str,
    settings: DurableSettings,
) -> Node {
    let config = settings.validate().expect("the law's settings validate");
    let (send, arrived) = mpsc::unbounded_channel();
    let mut runner = Runner::new(
        store,
        Arc::new(lash_core_execution::runtime::SystemClock),
        RunnerConfig::new(NodeId::new(name), vec![formats()], &config),
        Arc::new(Hold { arrived: send }),
    );
    if let Some(signals) = signals {
        runner = runner.with_signals(signals);
    }
    let hints = runner.hints();
    let task = tokio::spawn(runner.run(std::future::pending()));
    Node {
        hints,
        arrived,
        task,
    }
}

/// Every claim call the counted nodes made, as (node, actors taken).
#[derive(Clone, Default)]
struct Claims(Arc<std::sync::Mutex<Vec<(String, usize)>>>);

impl Claims {
    fn len(&self) -> usize {
        self.0.lock().expect("the claims lock").len()
    }

    fn take(&self) -> Vec<(String, usize)> {
        std::mem::take(&mut *self.0.lock().expect("the claims lock"))
    }
}

/// The seam the wake laws count claim attempts through: a durable store
/// that records each claim call and forwards every call unchanged.
struct CountingStore {
    inner: PostgresDurableStore,
    node: String,
    claims: Claims,
}

#[async_trait::async_trait]
impl DurableStore for CountingStore {
    async fn now(&self) -> Result<lash_durable::DurableInstant, DurableError> {
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

    async fn claim(
        &self,
        node: &NodeLease,
        limit: usize,
    ) -> Result<Vec<lash_durable::Claimed>, DurableError> {
        let claimed = self.inner.claim(node, limit).await;
        if let Ok(claimed) = &claimed {
            self.claims
                .0
                .lock()
                .expect("the claims lock")
                .push((self.node.clone(), claimed.len()));
        }
        claimed
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<lash_durable::Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(
        &self,
        actor: &ActorKey,
        epoch: lash_durable::Epoch,
    ) -> Result<lash_durable::ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn commit(
        &self,
        tx: lash_durable::ActorTx,
        label: CommitLabel,
    ) -> Result<lash_durable::ActorCommit, DurableError> {
        self.inner.commit(tx, label).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<lash_durable::MailCommit, DurableError> {
        self.inner.commit_mail(tx, label).await
    }

    async fn actor(
        &self,
        actor: &ActorKey,
    ) -> Result<Option<lash_durable::ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }
}

#[async_trait::async_trait]
impl lash_durable::DurableReads for CountingStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<domain::TurnRow>, DurableError> {
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

async fn owner_of(store: &dyn DurableStore, actor: &ActorKey) -> Option<String> {
    store
        .actor(actor)
        .await
        .expect("read the actor")
        .and_then(|snapshot| snapshot.owner)
        .map(|owner| owner.node.as_str().to_owned())
}

/// A listener holds its boot's liveness lock. Only a reaper that itself
/// listens may reap through the lock, and only once the lock is free: then
/// the boot is reaped at once, long before its lease lapses, with its actors'
/// epochs bumped, so the dead boot's zombie commit is refused and leaves
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_liveness_lock_is_reaped_at_once_and_fences_the_zombie() {
    let Some(database) =
        database("a_released_liveness_lock_is_reaped_at_once_and_fences_the_zombie").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let held_actor = actor("held");
    create(&store, &held_actor).await;
    let dead = node(&store, "dead").await;
    let watcher = node(&store, "watcher").await;
    let claimed = store.claim(&dead, 1).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    let feed = signals.listen(&dead).await.expect("listen");
    assert_eq!(held(&signals, &dead.owner).await, Some(true));
    assert_eq!(held(&signals, &watcher.owner).await, Some(false));
    assert!(
        signals
            .reap_released(&watcher, &dead.owner)
            .await
            .expect("reap")
            .is_empty(),
        "a reaper that holds no lock of its own reaped"
    );
    let _watching = signals.listen(&watcher).await.expect("listen");
    assert!(
        signals
            .reap_released(&watcher, &dead.owner)
            .await
            .expect("reap")
            .is_empty(),
        "a boot whose lock is held was reaped"
    );
    let mut zombie = store
        .begin(&held_actor, claimed[0].epoch)
        .await
        .expect("the owner opens");
    zombie.ack_seen().give_up(Release::Idle);

    drop(feed);
    eventually(Duration::from_secs(5), "the lock is released", || async {
        held(&signals, &dead.owner).await == Some(false)
    })
    .await;
    let reaped = signals
        .reap_released(&watcher, &dead.owner)
        .await
        .expect("reap");
    assert!(
        reaped.len() == 1
            && reaped[0].actor == held_actor
            && reaped[0].from == dead.owner
            && reaped[0].epoch > claimed[0].epoch,
        "the released boot's reap answered {reaped:?}"
    );
    assert_eq!(
        store.heartbeat(&dead).await.expect("heartbeat"),
        HeartbeatOutcome::Reaped
    );
    let before = store.actor(&held_actor).await.expect("read");
    assert!(matches!(
        store.commit(zombie, CommitLabel::new("law.write")).await,
        Err(DurableError::OwnershipLost(_))
    ));
    let after = store.actor(&held_actor).await.expect("read");
    assert_eq!(before, after, "the zombie's refused commit left a trace");
    assert!(after.is_some_and(|after| after.state == ActorState::Ready));
}

/// A listener whose session the server ends opens another, subscribes and
/// takes its lock again, and only then reports `Resubscribed`; hints sent
/// after that reach it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_listener_session_resubscribes_holding_its_lock() {
    let Some(database) = database("a_lost_listener_session_resubscribes_holding_its_lock").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let lease = node(&store, "blip").await;
    let mut feed = signals.listen(&lease).await.expect("listen");
    let ended: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks
         WHERE locktype = 'advisory' AND granted
           AND classid = 1818325864 AND objid = hashtext($1)::oid",
    )
    .bind(lease.owner.boot.as_str())
    .fetch_all(storage.pool())
    .await
    .expect("end the listener's session");
    assert_eq!(ended, vec![true], "one session held the boot's lock");
    let signal = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .expect("the listener reports its new session");
    assert_eq!(signal, Signal::Resubscribed);
    assert_eq!(feed.session(), 1);
    assert_eq!(held(&signals, &lease.owner).await, Some(true));
    signals
        .publish(&WakeBatch {
            ready: std::collections::BTreeSet::from([lease.owner.node.clone()]),
            ..WakeBatch::default()
        })
        .await
        .expect("publish");
    let signal = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .expect("a hint reaches the new session");
    assert_eq!(signal, Signal::Ready);
}

/// O1: mail written on node B to an actor hot on node A reaches A through
/// B's after-commit hint, far inside A's ten-second mail poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_from_another_node_reaches_a_hot_owner_through_its_hint() {
    let Some(database) =
        database("mail_from_another_node_reaches_a_hot_owner_through_its_hint").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let hot = actor("hot");
    create(&store, &hot).await;
    let slow_poll = LeaseSettings {
        claim_poll: Duration::from_secs(10),
        ..LeaseSettings::default()
    };
    let mut owner = start(&storage, "a", slow_poll, true);
    eventually(Duration::from_secs(5), "node a owns the actor", || async {
        owner_of(&store, &hot).await.as_deref() == Some("a")
    })
    .await;
    let writer = start(&storage, "b", slow_poll, true);
    eventually(Duration::from_secs(5), "node b listens", || async {
        signals
            .liveness()
            .await
            .expect("probe")
            .iter()
            .any(|liveness| liveness.boot.node.as_str() == "b" && liveness.held)
    })
    .await;

    let sent = Instant::now();
    let commit = store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await
        .expect("mail");
    writer.hints.woke(&commit);
    let arrived = tokio::time::timeout(Duration::from_secs(5), owner.arrived.recv())
        .await
        .expect("the mail arrives")
        .expect("the owner runs");
    let latency = arrived.saturating_duration_since(sent);
    eprintln!("cross-node hint: mail seen after {latency:?}");
    assert!(
        latency < Duration::from_secs(1),
        "the hint took {latency:?}, as long as a poll"
    );
    owner.task.abort();
    writer.task.abort();
}

/// O1: mail whose hint is lost (a writer with no signals, standing in for a
/// dropped NOTIFY) still reaches a hot owner within its mail poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll() {
    let Some(database) =
        database("mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let hot = actor("hot");
    create(&store, &hot).await;
    let poll = Duration::from_millis(500);
    let lease = LeaseSettings {
        claim_poll: poll,
        ..LeaseSettings::default()
    };
    let mut owner = start(&storage, "a", lease, true);
    eventually(Duration::from_secs(5), "node a owns the actor", || async {
        owner_of(&store, &hot).await.as_deref() == Some("a")
    })
    .await;
    let sent = Instant::now();
    store
        .commit_mail(mail(&hot), CommitLabel::MAIL_SESSION)
        .await
        .expect("mail");
    let arrived = tokio::time::timeout(Duration::from_secs(5), owner.arrived.recv())
        .await
        .expect("the mail arrives")
        .expect("the owner runs");
    let latency = arrived.saturating_duration_since(sent);
    eprintln!("lost hint: mail seen after {latency:?} on a {poll:?} poll");
    assert!(
        latency < poll + Duration::from_millis(500),
        "the poll took {latency:?}"
    );
    owner.task.abort();
}

/// A node that dies is reaped through its liveness lock, and its actor is
/// claimed by a surviving node, in a small fraction of the fifteen-second
/// lease a lease reap would wait for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses() {
    let Some(database) =
        database("a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses").await
    else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let hot = actor("hot");
    create(&store, &hot).await;
    let dying = start(&storage, "dying", LeaseSettings::default(), true);
    eventually(
        Duration::from_secs(5),
        "the dying node owns the actor",
        || async { owner_of(&store, &hot).await.as_deref() == Some("dying") },
    )
    .await;
    let survivor = start(&storage, "survivor", LeaseSettings::default(), true);
    eventually(Duration::from_secs(5), "both nodes listen", || async {
        let live = signals.liveness().await.expect("probe");
        live.len() == 2 && live.iter().all(|liveness| liveness.held)
    })
    .await;
    // Let the survivor's watch see the dying node's lock held.
    tokio::time::sleep(Duration::from_millis(600)).await;

    dying.task.abort();
    let failover = eventually(
        Duration::from_secs(10),
        "the survivor takes over",
        || async { owner_of(&store, &hot).await.as_deref() == Some("survivor") },
    )
    .await;
    eprintln!("lock failover: the survivor owns the actor after {failover:?}");
    assert!(
        failover < Duration::from_secs(3),
        "the takeover took {failover:?}, near the lease"
    );
    survivor.task.abort();
}

/// Polls far apart, so inside a wake law every claim is a hint's.
fn quiet() -> LeaseSettings {
    LeaseSettings {
        claim_poll: Duration::from_secs(30),
        claim_backoff: Duration::from_secs(30),
        ..LeaseSettings::default()
    }
}

/// Settings that run at most `max_active` actors under `lease`.
fn holding(lease: LeaseSettings, max_active: usize) -> DurableSettings {
    let defaults = DurableSettings::default();
    DurableSettings {
        lease,
        max_active,
        claim_batch: defaults.claim_batch.min(max_active),
        ..defaults
    }
}

/// Create `actor` ready and unowned, answering what the commit woke.
async fn create_woken(store: &dyn DurableStore, actor: &ActorKey) -> lash_durable::MailCommit {
    let mut tx = MailTx::new();
    tx.create_actor(actor.clone(), formats());
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await
        .expect("create the actor")
}

/// Wait until `node` holds its liveness lock.
async fn listening(signals: &PostgresSignals, node: &str) {
    eventually(Duration::from_secs(5), "the node listens", || async {
        signals
            .liveness()
            .await
            .expect("probe")
            .iter()
            .any(|liveness| liveness.boot.node.as_str() == node && liveness.held)
    })
    .await;
}

/// A readied unowned actor rings one node, not every node (FIG-5277). The
/// producing node is full, so its hint goes to one of fifteen peers; each
/// readied actor then costs exactly one claim attempt, and that attempt
/// takes it. Before, every peer claimed on the shared ready channel and
/// all but one came back empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_readied_actor_is_claimed_by_one_attempt_on_one_node() {
    const NODES: usize = 16;
    const READIED: usize = 8;
    let Some(database) = database("a_readied_actor_is_claimed_by_one_attempt_on_one_node").await
    else {
        return;
    };
    let mut config = crate::testing::fixture_config();
    config.roles.served_nodes = u32::try_from(NODES).expect("the node count fits");
    config.roles.scheduler.max_connections = 4;
    let storage = crate::testing::connect_with(database.url(), &config)
        .await
        .expect("open the isolated store");
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let claims = Claims::default();
    let mut nodes = Vec::new();
    for index in 1..NODES {
        let name = format!("peer-{index:02}");
        nodes.push(start_counted(
            &storage,
            &name,
            holding(quiet(), 256),
            &claims,
        ));
        listening(&signals, &name).await;
    }
    eventually(Duration::from_secs(5), "each peer claimed once", || async {
        claims.len() == NODES - 1
    })
    .await;
    let held_actor = actor("held");
    create(&store, &held_actor).await;
    let producer = start_counted(&storage, "producer", holding(quiet(), 1), &claims);
    eventually(Duration::from_secs(5), "the producer is full", || async {
        owner_of(&store, &held_actor).await.as_deref() == Some("producer")
    })
    .await;
    claims.take();

    for index in 0..READIED {
        let readied = actor(&format!("readied-{index}"));
        producer.hints.woke(&create_woken(&store, &readied).await);
        eventually(Duration::from_secs(5), "a peer runs the actor", || async {
            owner_of(&store, &readied)
                .await
                .is_some_and(|owner| owner.starts_with("peer-"))
        })
        .await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let attempts = claims.take();
    let empty = attempts.iter().filter(|(_, taken)| *taken == 0).count();
    eprintln!(
        "ready wake on {NODES} nodes: {} claim attempts for {READIED} readied actors, \
         {empty} empty",
        attempts.len()
    );
    assert_eq!(
        (attempts.len(), empty),
        (READIED, 0),
        "each readied actor costs one claim attempt that takes it: {attempts:?}"
    );
    producer.task.abort();
    for node in nodes {
        node.task.abort();
    }
}

/// The hinted node dies before it claims: its hint is lost, and a live
/// node's claim poll takes the actor within one poll interval. The full
/// producer last probed liveness while the dead node was its only live
/// peer, so the hint goes to the dead node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hint_to_a_dead_node_is_backed_by_the_claim_poll() {
    let Some(database) = database("a_hint_to_a_dead_node_is_backed_by_the_claim_poll").await else {
        return;
    };
    let storage = storage(&database).await;
    let store = storage.durable_store();
    let signals = storage.durable_signals();
    let claims = Claims::default();
    let doomed = start_counted(&storage, "doomed", holding(quiet(), 256), &claims);
    listening(&signals, "doomed").await;
    eventually(
        Duration::from_secs(5),
        "the doomed node claimed",
        || async { claims.len() == 1 },
    )
    .await;
    let held_actor = actor("held");
    create(&store, &held_actor).await;
    let producer = start_counted(&storage, "producer", holding(quiet(), 1), &claims);
    eventually(Duration::from_secs(5), "the producer is full", || async {
        owner_of(&store, &held_actor).await.as_deref() == Some("producer")
    })
    .await;
    let poll = Duration::from_millis(500);
    let polling = LeaseSettings {
        claim_poll: poll,
        claim_backoff: poll,
        ..LeaseSettings::default()
    };
    let survivor = start_counted(&storage, "survivor", holding(polling, 256), &claims);
    listening(&signals, "survivor").await;
    doomed.task.abort();
    assert!(doomed.task.await.is_err(), "the doomed node was stopped");
    eventually(
        Duration::from_secs(5),
        "the doomed node's lock is free",
        || async {
            signals
                .liveness()
                .await
                .expect("probe")
                .iter()
                .all(|liveness| liveness.boot.node.as_str() != "doomed" || !liveness.held)
        },
    )
    .await;

    let readied = actor("readied");
    let sent = Instant::now();
    producer.hints.woke(&create_woken(&store, &readied).await);
    eventually(
        Duration::from_secs(5),
        "the survivor runs the actor",
        || async { owner_of(&store, &readied).await.as_deref() == Some("survivor") },
    )
    .await;
    let latency = sent.elapsed();
    eprintln!("lost ready hint: the poll claimed the actor after {latency:?}");
    assert!(
        latency < poll + Duration::from_millis(500),
        "the poll took {latency:?} on a {poll:?} poll"
    );
    producer.task.abort();
    survivor.task.abort();
}

/// A node's lease renews on a task of its own over its renewal connection:
/// with every work, scheduler and critical connection held, so the runner's
/// claim waits on the scheduler pool, the node keeps serving across three
/// self-stop windows, so it renewed at least three times. Its stored lease
/// lives two seconds and a watcher on a second storage reaps expired leases
/// every 250 ms, so those renewals reached the database: a reaped node's
/// next heartbeat answers `Reaped` and its runner stops (FIG-5241,
/// FIG-5240).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturated_shared_pool_cannot_starve_the_heartbeat() {
    let Some(database) = database("a_saturated_shared_pool_cannot_starve_the_heartbeat").await
    else {
        return;
    };
    let storage = crate::testing::connect_with(database.url(), &crate::testing::work_pool_of(2))
        .await
        .expect("open the isolated store");
    let store = storage.durable_store();
    let watching = crate::testing::connect(database.url())
        .await
        .expect("open the watcher's store");
    let watching = watching.durable_store();
    let lease = LeaseSettings {
        ttl: Duration::from_secs(2),
        heartbeat_every: Duration::from_millis(300),
        self_stop_after: Duration::from_millis(1_500),
        ..LeaseSettings::default()
    };
    let watcher = watching
        .register_node(&NodeSpec {
            node: NodeId::new("watcher"),
            decodes: vec![formats()],
            ttl_millis: i64::try_from(lease.ttl.as_millis()).expect("ttl fits"),
        })
        .await
        .expect("register the watcher");
    let pools = &store.pools;
    let mut held = Vec::new();
    for pool in [&pools.work, &pools.scheduler, &pools.critical] {
        for _ in 0..pool.options().get_max_connections() {
            held.push(pool.acquire().await.expect("hold a connection"));
        }
    }
    let node = start(&storage, "busy", lease, false);
    let started = Instant::now();
    while started.elapsed() < 3 * lease.self_stop_after {
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            matches!(
                watching.heartbeat(&watcher).await,
                Ok(HeartbeatOutcome::Renewed { .. })
            ),
            "the watcher renews"
        );
        watching.reap(&watcher).await.expect("reap expired leases");
        assert!(
            !node.task.is_finished(),
            "the busy node stopped {:?} in: {:?}",
            started.elapsed(),
            node.task.await
        );
    }
    // A reap in the last tick ends the runner at its next heartbeat.
    tokio::time::sleep(2 * lease.heartbeat_every).await;
    assert!(
        !node.task.is_finished(),
        "the busy node's lease was reaped: {:?}",
        node.task.await
    );
    node.task.abort();
}

#[test]
fn a_batch_rings_each_channel_with_bounded_payloads() {
    let mut batch = WakeBatch {
        ready: std::collections::BTreeSet::from([NodeId::new("b")]),
        ..WakeBatch::default()
    };
    let crowd: std::collections::BTreeSet<ActorKey> = (0..1_000)
        .map(|index| actor(&format!("crowded-{index:04}")))
        .collect();
    batch.owned.insert(NodeId::new("a"), crowd.clone());
    let (channels, payloads) = notifications(&batch);
    assert_eq!(
        (channels[0].as_str(), payloads[0].as_str()),
        ("lash_node_b", ""),
        "a ready hint rings its one node with an empty payload"
    );
    assert!(channels[1..].iter().all(|channel| channel == "lash_node_a"));
    assert!(
        payloads
            .iter()
            .all(|payload| payload.len() <= PAYLOAD_LIMIT)
    );
    let carried: std::collections::BTreeSet<ActorKey> = payloads[1..]
        .iter()
        .flat_map(|payload| payload.split('\n'))
        .map(|key| ActorKey::parse(key).expect("a carried key"))
        .collect();
    assert_eq!(carried, crowd, "every woken actor rides some payload");
    let long = NodeId::new("a node name that cannot be a channel identifier as it stands");
    assert!(node_channel(&long).len() <= 63);
}
