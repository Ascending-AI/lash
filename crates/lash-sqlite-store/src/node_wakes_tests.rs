//! Laws of node wakes over a SQLite database file (FIG-5422): the node-wake
//! laws every dialect keeps (`lash_durable::laws::node_wakes`), each node on
//! store handles of its own over the one file; and the laws only separate OS
//! processes can show, run by this test binary re-executing itself as a
//! node: a wake crosses processes, a killed process's lock
//! is released at once, processes writing one file at once lose and double
//! no work, a sweep pass's liveness lock is seen from every process, and a
//! grouped commit killed before or after its `COMMIT` is all or nothing.

// Test code: the laws spawn this test binary and read its environment.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_durable::laws::node_wakes::{NodeWakeTier, serve_node};
use lash_durable::runner::{Activation, Exit, Owned, Runner, RunnerConfig};
use lash_durable::{
    ActorKey, BootLiveness, CommitLabel, DurableError, DurableSettings, DurableStore, FormatSet,
    LeaseSettings, MailKind, MailTx, NodeId, NodeLease, NodeWakeEvent, NodeWakeFeed, NodeWakes,
    Owner, Reaped, WakeBatch,
};
use lash_execution_contracts::backend::StoreSet as _;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, watch};

use super::*;
use crate::SqliteStoreSet;

/// A database file and the store set the laws sever listeners through.
struct SqliteTier {
    stores: SqliteStoreSet,
    _dir: tempfile::TempDir,
}

impl SqliteTier {
    async fn open() -> Self {
        let dir = tempfile::tempdir().expect("a database directory");
        let stores =
            SqliteStoreSet::open(dir.path().join("lash.db"), crate::SqliteSynchronous::Normal)
                .await
                .expect("open the database file");
        Self { stores, _dir: dir }
    }
}

#[async_trait::async_trait]
impl NodeWakeTier for SqliteTier {
    /// Each node opens the file afresh, with connections of its own, as a
    /// process of its own would.
    async fn open(&self) -> (Arc<dyn DurableStore>, Arc<dyn NodeWakes>) {
        let stores = self
            .stores
            .reopen()
            .await
            .expect("reopen the database file");
        let node_wakes = stores.node_wakes().expect("a database file has node wakes");
        (Arc::new(stores.durable_store()), node_wakes)
    }

    async fn sever(&self, boot: &Owner) {
        self.stores
            .sqlite_node_wakes_for_testing()
            .expect("a database file has node wakes")
            .sever_for_testing(&boot.boot);
    }
}

macro_rules! node_wake_laws {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            lash_durable::laws::node_wakes::$name(&SqliteTier::open().await)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    )*};
}

node_wake_laws!(
    a_released_liveness_lock_is_reaped_at_once_and_fences_the_zombie,
    a_lost_listener_session_resubscribes_holding_its_lock,
    an_appended_log_is_named_to_every_listening_node,
    mail_from_another_node_reaches_a_hot_owner_through_its_hint,
    mail_for_an_oversized_key_reaches_a_hot_owner_through_a_store_scan_hint,
    a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses,
);

#[path = "node_wakes_commit_tests.rs"]
mod commit;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_readied_actor_is_claimed_by_one_attempt_on_one_node() {
    commit::one_claim_per_readied_actor().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hint_to_a_dead_node_is_backed_by_the_claim_poll() {
    commit::lost_hint_reaches_the_claim_poll().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll() {
    commit::lost_owned_hint_reaches_the_mail_poll().await;
}

// ---- Nodes in processes of their own ----

/// The variable that turns this test binary into a child node: its role,
/// then the database file, separated by a newline.
const CHILD: &str = "LASH_SQLITE_NODE_CHILD";

/// What a child prints before each report, so the parent can tell its
/// reports from the test harness's own output.
const REPORT: &str = "lash-node-report ";

fn formats() -> FormatSet {
    FormatSet::new("law-formats")
}

fn actor(name: &str) -> ActorKey {
    ActorKey::session(name).expect("a law's actor key")
}

/// The cross-process law never advances store time: a lease cannot expire,
/// so takeover must use the released liveness lock.
const STORE_TIME: u64 = 1_700_000_000_000;
/// A hang guard, never an assertion about latency.
const WATCHDOG: Duration = Duration::from_secs(60);

fn report(line: &str) {
    println!("{REPORT}{line}");
}

/// The role this process plays, when it was started as a child node.
fn child_role() -> Option<(String, PathBuf)> {
    let value = std::env::var(CHILD).ok()?;
    let (role, path) = value.split_once('\n')?;
    Some((role.to_owned(), PathBuf::from(path)))
}

/// One child node process, the report lines it prints, and what it wrote
/// to stderr for a failure's panic.
struct Child {
    process: tokio::process::Child,
    reports: mpsc::UnboundedReceiver<String>,
    stderr: Arc<Mutex<String>>,
}

impl Child {
    /// Start this test binary as child node `role` over `database`, running
    /// test `test` alone.
    fn start(test: &str, role: &str, database: &Path) -> Self {
        let mut process =
            tokio::process::Command::new(std::env::current_exe().expect("the test binary's path"))
                .args(["--exact", test, "--nocapture", "--test-threads", "1"])
                .env(CHILD, format!("{role}\n{}", database.display()))
                .stdin(if test == CONTENTION {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("the child node starts");
        let stdout = process.stdout.take().expect("piped stdout");
        let stderr_pipe = process.stderr.take().expect("piped stderr");
        let (send, reports) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // The harness may have begun the line with the test's name.
                if let Some((_, report)) = line.split_once(REPORT) {
                    let _ = send.send(report.to_owned());
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        tokio::spawn({
            let stderr = Arc::clone(&stderr);
            async move {
                let mut lines = tokio::io::BufReader::new(stderr_pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut captured = stderr.lock().expect("the child's stderr buffer");
                    captured.push_str(&line);
                    captured.push('\n');
                }
            }
        });
        Self {
            process,
            reports,
            stderr,
        }
    }

    /// What the child wrote to stderr so far.
    fn stderr(&self) -> String {
        self.stderr
            .lock()
            .expect("the child's stderr buffer")
            .clone()
    }

    /// The next report line, within `within`.
    async fn next(&mut self, within: Duration) -> String {
        match tokio::time::timeout(within, self.reports.recv()).await {
            Ok(Some(line)) => line,
            Ok(None) => panic!("the child is running; its stderr:\n{}", self.stderr()),
            Err(_) => panic!("the child reports in time; its stderr:\n{}", self.stderr()),
        }
    }

    /// Release this contention node's held claims after mail is queued.
    async fn release_tallies(&mut self) {
        self.process
            .stdin
            .as_mut()
            .expect("a contention node's piped stdin")
            .write_all(b"tally\n")
            .await
            .expect("release the contention node's tallies");
    }

    /// Kill the child with SIGKILL and wait for it to be gone.
    async fn kill(&mut self) {
        self.process
            .start_kill()
            .unwrap_or_else(|error| panic!("the child is killed: {error}\n{}", self.stderr()));
        self.process.wait().await.unwrap_or_else(|error| {
            panic!("the killed child is reaped: {error}\n{}", self.stderr())
        });
    }
}

/// Holds every actor it claims hot and reports each mail it acknowledged.
struct Report;

#[async_trait::async_trait]
impl Activation for Report {
    async fn activate(&self, owned: Owned) -> Exit {
        report(&format!("owns {}", owned.actor()));
        loop {
            let Ok(mut tx) = owned.begin().await else {
                return Exit::Released;
            };
            if !tx.mail().is_empty() {
                let seqs: Vec<i64> = tx.mail().iter().map(|mail| mail.seq.0).collect();
                tx.ack_seen();
                if owned.commit(tx, CommitLabel::new("law.ack")).await.is_err() {
                    return Exit::Released;
                }
                for seq in seqs {
                    report(&format!("acked {} {seq}", owned.actor()));
                }
            }
            owned.wait_for_mail().await;
        }
    }
}

/// Acknowledges each mail it reads, reports it, and releases its actor idle,
/// so the next mail readies it for whichever node claims first. Holds the
/// initial claims until the parent has observed both nodes owning actors and
/// queued mail for them; later activations pass through the released gate.
struct Tally {
    released: watch::Receiver<bool>,
}

#[async_trait::async_trait]
impl Activation for Tally {
    async fn activate(&self, owned: Owned) -> Exit {
        report(&format!("owns {}", owned.actor()));
        self.released
            .clone()
            .wait_for(|released| *released)
            .await
            .expect("the parent releases the tally gate");
        let Ok(mut tx) = owned.begin().await else {
            return Exit::Released;
        };
        let seqs: Vec<i64> = tx.mail().iter().map(|mail| mail.seq.0).collect();
        tx.ack_seen().give_up(lash_durable::Release::Idle);
        match owned.commit(tx, CommitLabel::new("law.tally")).await {
            Ok(_) => {
                for seq in seqs {
                    report(&format!("acked {} {seq}", owned.actor()));
                }
                Exit::Released
            }
            Err(_) => Exit::Abandoned,
        }
    }
}

/// The attachment the sweeper child condemns.
fn swept() -> lash_protocol_types::attachment::AttachmentId {
    lash_protocol_types::attachment::AttachmentId::parse("a1".repeat(32)).expect("an attachment id")
}

/// Facts observed at the production runner's node-wake boundary.
#[derive(Debug)]
enum WakeEvidence {
    Listening(NodeLease),
    Watched(Vec<BootLiveness>),
    Woke(Vec<ActorKey>),
    Dropped(Vec<ActorKey>),
    Reaped(Vec<Reaped>),
}

/// Forwards every operation unchanged and records what the runner actually
/// observed, rather than guessing its progress from elapsed wall time.
struct ObservedWakes {
    inner: Arc<dyn NodeWakes>,
    evidence: mpsc::UnboundedSender<WakeEvidence>,
    drop_owned: bool,
}

struct ObservedFeed {
    inner: Box<dyn NodeWakeFeed>,
    evidence: mpsc::UnboundedSender<WakeEvidence>,
    drop_owned: bool,
}

#[async_trait::async_trait]
impl NodeWakeFeed for ObservedFeed {
    async fn next(&mut self) -> NodeWakeEvent {
        loop {
            let event = self.inner.next().await;
            if let NodeWakeEvent::Owned(actors) = &event {
                if self.drop_owned {
                    let _ = self.evidence.send(WakeEvidence::Dropped(actors.clone()));
                    continue;
                }
                let _ = self.evidence.send(WakeEvidence::Woke(actors.clone()));
            }
            return event;
        }
    }

    fn session(&self) -> u64 {
        self.inner.session()
    }
}

#[async_trait::async_trait]
impl NodeWakes for ObservedWakes {
    async fn publish(&self, batch: &WakeBatch) -> Result<(), DurableError> {
        self.inner.publish(batch).await
    }

    async fn committed(&self, batch: &WakeBatch) -> Result<(), DurableError> {
        self.inner.committed(batch).await
    }

    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        let inner = self.inner.listen(lease).await?;
        let _ = self.evidence.send(WakeEvidence::Listening(lease.clone()));
        Ok(Box::new(ObservedFeed {
            inner,
            evidence: self.evidence.clone(),
            drop_owned: self.drop_owned,
        }))
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

async fn evidence_until(
    evidence: &mut mpsc::UnboundedReceiver<WakeEvidence>,
    reached: impl Fn(&WakeEvidence) -> bool + Send + Sync,
) -> WakeEvidence {
    // What the runner produced instead, so a hang names the wait it broke.
    let mut passed = Vec::new();
    let found = tokio::time::timeout(WATCHDOG, async {
        loop {
            let event = evidence
                .recv()
                .await
                .expect("the runner keeps its evidence channel open");
            if reached(&event) {
                return event;
            }
            passed.push(event);
        }
    })
    .await;
    found.unwrap_or_else(|_| {
        let last: Vec<_> = passed.iter().rev().take(4).rev().collect();
        panic!(
            "hang guard: the runner never produced the required node-wake evidence; \
             its last of {} other events: {last:#?}",
            passed.len()
        )
    })
}

/// Serve child node `role` over `database` until killed; the `sweeper`
/// instead holds a sweep pass that condemned one attachment.
async fn run_child(role: &str, database: &Path) {
    let stores = if role == "hot" {
        SqliteStoreSet::open_with_clock(
            database,
            crate::SqliteSynchronous::Normal,
            Arc::new(lash_core::testing::TestClock::new(STORE_TIME)),
        )
        .await
    } else {
        SqliteStoreSet::open(database, crate::SqliteSynchronous::Normal).await
    }
    .expect("the child opens the database file");
    if role == "sweeper" {
        let store = stores.session_store_factory();
        let pass = store
            .begin_attachment_sweep()
            .await
            .expect("begin a sweep pass");
        store
            .condemn_attachment(&swept(), &pass)
            .await
            .expect("condemn the attachment");
        report("condemned");
        std::future::pending::<()>().await;
    }
    let node_wakes = if role == "hot" {
        let (send, mut evidence) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = evidence.recv().await {
                if let WakeEvidence::Woke(actors) = event {
                    for actor in actors {
                        report(&format!("woke {actor}"));
                    }
                }
            }
        });
        let observed: Arc<dyn NodeWakes> = Arc::new(ObservedWakes {
            inner: stores.node_wakes().expect("file node wakes"),
            evidence: send,
            drop_owned: false,
        });
        Some(observed)
    } else {
        stores.node_wakes()
    };
    let (activation, settings): (Arc<dyn Activation>, DurableSettings) = match role {
        // Wake delivery is observed explicitly, independently of the mail poll.
        "hot" => (
            Arc::new(Report),
            DurableSettings {
                lease: LeaseSettings {
                    claim_poll: Duration::from_secs(10),
                    ..LeaseSettings::default()
                },
                ..DurableSettings::default()
            },
        ),
        _ => {
            let (release, released) = watch::channel(false);
            tokio::task::spawn_blocking(move || {
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .expect("read the parent's tally gate");
                assert_eq!(line, "tally\n", "the parent releases the held claims");
                release.send(true).expect("the tally gate stays open");
            });
            (
                Arc::new(Tally { released }),
                DurableSettings {
                    max_active: CONTENTION_ACTIVE,
                    claim_batch: CONTENTION_ACTIVE,
                    ..DurableSettings::default()
                },
            )
        }
    };
    let config = settings.validate().expect("the child's settings validate");
    let mut runner = Runner::new(
        Arc::new(stores.durable_store()),
        Arc::new(lash_core_ids::clock::SystemClock),
        RunnerConfig::new(NodeId::new(role), vec![formats()], &config),
        activation,
    );
    if let Some(node_wakes) = node_wakes {
        runner = runner.with_node_wakes(node_wakes);
    }
    report("serving");
    let stopped = runner.run(std::future::pending()).await;
    report(&format!("stopped {stopped:?}"));
}

const CROSS_PROCESS: &str = "durable::node_wakes::tests::a_wake_crosses_processes_and_a_killed_process_is_reaped_by_its_lock";

/// Two processes serve one database file. Each mail produces an Owned wake
/// on the child's listener and is acknowledged by its actor. After the
/// survivor has observed the child's held lock, SIGKILL releases it and the
/// runner reaps that exact boot through reap_released, then claims its actor
/// at a later epoch. Store time is fixed in both processes: lease expiry
/// cannot satisfy the takeover. Timeouts only guard hangs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wake_crosses_processes_and_a_killed_process_is_reaped_by_its_lock() {
    if let Some((role, database)) = child_role() {
        return run_child(&role, &database).await;
    }
    const WAKES: usize = 20;
    let dir = tempfile::tempdir().expect("a database directory");
    let database = dir.path().join("lash.db");
    let stores = SqliteStoreSet::open_with_clock(
        &database,
        crate::SqliteSynchronous::Normal,
        Arc::new(lash_core::testing::TestClock::new(STORE_TIME)),
    )
    .await
    .expect("open the database file on frozen store time");
    let store = stores.durable_store();
    let hot = actor("hot");
    let mut tx = MailTx::new();
    tx.create_actor(hot.clone(), formats());
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await
        .expect("create the actor");
    let mut child = Child::start(CROSS_PROCESS, "hot", &database);
    assert_eq!(child.next(WATCHDOG).await, "serving");
    assert_eq!(child.next(WATCHDOG).await, format!("owns {hot}"));
    let claimed = store.actor(&hot).await.expect("read the actor").unwrap();
    let child_boot = claimed.owner.expect("the child owns the actor");
    let (send, mut evidence) = mpsc::unbounded_channel();
    let parent = serve_node(
        Arc::new(store.clone()),
        Some(Arc::new(ObservedWakes {
            inner: stores.node_wakes().expect("file node wakes"),
            evidence: send,
            drop_owned: false,
        })),
        "parent",
        DurableSettings::default(),
    );
    // This is the survivor's actual probe, not a separate observer's probe.
    // Once it has returned, the runner records the held boot before it can
    // process the next probe, so killing now cannot race initial discovery.
    evidence_until(&mut evidence, |event| {
        matches!(event, WakeEvidence::Watched(boots)
            if boots.iter().any(|boot| boot.boot == child_boot && boot.held)
                && boots.iter().any(|boot| boot.boot.node.as_str() == "parent" && boot.held))
    })
    .await;

    for _ in 0..WAKES {
        let mut tx = MailTx::new();
        tx.append(hot.clone(), MailKind::new("law.note"), "note");
        let commit = store
            .commit_mail(tx, CommitLabel::MAIL_SESSION)
            .await
            .expect("mail the hot actor");
        assert_eq!(commit.appended.len(), 1, "one mail per wake");
        let (actor, seq) = &commit.appended[0];
        assert_eq!(actor, &hot);
        parent.hints.woke(&commit);
        let mut woke = false;
        let mut acked = false;
        while !woke || !acked {
            let line = child.next(WATCHDOG).await;
            if line == format!("woke {hot}") {
                assert!(!woke, "one cross-process wake per mail");
                woke = true;
            } else {
                assert_eq!(line, format!("acked {hot} {}", seq.0));
                assert!(!acked, "one acknowledgement per mail");
                acked = true;
            }
        }
    }

    // One publication spans several envelopes, and every actor must reach
    // the listener in the other process through SQLite's forwarding path.
    let actors: BTreeSet<ActorKey> = (0..1_000)
        .map(|index| actor(&format!("crowded-{index:04}")))
        .collect();
    assert!(
        node_wake_payload::owned(&actors).len() > 1,
        "the crowd must span multiple wake envelopes"
    );
    stores
        .node_wakes()
        .expect("file node wakes")
        .publish(&WakeBatch {
            owned: BTreeMap::from([(child_boot.node.clone(), actors.clone())]),
            ..WakeBatch::default()
        })
        .await
        .expect("publish the entire crowd in one batch");
    let mut expected: BTreeSet<String> =
        actors.iter().map(|actor| format!("woke {actor}")).collect();
    while !expected.is_empty() {
        let line = child.next(WATCHDOG).await;
        assert!(
            expected.remove(&line),
            "unexpected or duplicate wake: {line}"
        );
    }

    child.kill().await;
    let WakeEvidence::Reaped(reaped) = evidence_until(&mut evidence, |event| {
        matches!(event, WakeEvidence::Reaped(_))
    })
    .await
    else {
        unreachable!("the predicate selects reaping evidence");
    };
    assert_eq!(reaped.len(), 1, "one actor released through the dead lock");
    assert_eq!(reaped[0].actor, hot);
    assert_eq!(reaped[0].from, child_boot);
    assert!(reaped[0].epoch > claimed.epoch, "the reap fences the child");
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let snapshot = store.actor(&hot).await.expect("read the actor").unwrap();
            if snapshot
                .owner
                .as_ref()
                .is_some_and(|owner| owner.node.as_str() == "parent")
            {
                assert!(snapshot.epoch > claimed.epoch, "takeover fences the child");
                break;
            }
            tokio::time::sleep(crate::SqliteOperationalSettings::standard().wake_poll).await;
        }
    })
    .await
    .expect("hang guard: the parent never took the killed child's actor over");
    assert_eq!(
        store.now().await.expect("read frozen store time").0,
        i64::try_from(STORE_TIME).expect("the law's timestamp fits")
    );
    parent.kill().await;
}

const CONTENTION: &str =
    "durable::node_wakes::tests::processes_writing_one_file_at_once_lose_and_double_no_work";
const CONTENTION_ACTIVE: usize = 4;

/// Three processes write one database file at once: this one appends mail
/// to many actors while two child nodes claim them, acknowledge each one's
/// mail and release it, all under SQLite's one writer and its busy timeout.
/// Every mail is acknowledged exactly once, by one node, and both nodes did
/// part of the work: each holds a full claim batch behind its stdin gate
/// until both own actors and the parent has queued the first mail round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_writing_one_file_at_once_lose_and_double_no_work() {
    if let Some((role, database)) = child_role() {
        return run_child(&role, &database).await;
    }
    const ACTORS: usize = 48;
    const ROUNDS: usize = 5;
    let tier = SqliteTier::open().await;
    let database = tier._dir.path().join("lash.db");
    let store = tier.stores.durable_store();
    let actors: Vec<ActorKey> = (0..ACTORS)
        .map(|index| actor(&format!("tally-{index:02}")))
        .collect();
    let mut tx = MailTx::new();
    for actor in &actors {
        tx.create_actor(actor.clone(), formats());
    }
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await
        .expect("create the actors");
    let mut nodes = [
        Child::start(CONTENTION, "left", &database),
        Child::start(CONTENTION, "right", &database),
    ];
    let mut held = BTreeSet::new();
    for node in &mut nodes {
        assert_eq!(node.next(Duration::from_secs(30)).await, "serving");
        for _ in 0..CONTENTION_ACTIVE {
            let line = node.next(Duration::from_secs(30)).await;
            let claimed = line.strip_prefix("owns ").expect("a held claim report");
            assert!(
                actors.iter().any(|actor| actor.to_string() == claimed),
                "a node claimed an actor outside the workload: {claimed}"
            );
            assert!(
                held.insert(claimed.to_owned()),
                "both nodes claimed {claimed}"
            );
        }
    }
    let mut appended = BTreeSet::new();
    for round in 0..ROUNDS {
        for actor in &actors {
            let mut tx = MailTx::new();
            tx.append(actor.clone(), MailKind::new("law.note"), "note");
            let commit = store
                .commit_mail(tx, CommitLabel::MAIL_SESSION)
                .await
                .expect("append under contention");
            for (actor, seq) in commit.appended {
                appended.insert((actor.to_string(), seq.0));
            }
        }
        if round == 0 {
            // Each node owns actors with mail before either can acknowledge
            // and release them. The remaining rounds race ordinary claims.
            for node in &mut nodes {
                node.release_tallies().await;
            }
        }
    }

    let mut acked: BTreeMap<(String, i64), Vec<&str>> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while acked.len() < appended.len() {
        assert!(
            Instant::now() < deadline,
            "{} of {} mails acknowledged",
            acked.len(),
            appended.len()
        );
        for (node, name) in nodes.iter_mut().zip(["left", "right"]) {
            while let Ok(line) = node.reports.try_recv() {
                let mut words = line.split(' ');
                if words.next() != Some("acked") {
                    continue;
                }
                let (Some(actor), Some(seq)) = (words.next(), words.next()) else {
                    panic!("an ack report: {line}");
                };
                let seq: i64 = seq.parse().expect("a mail position");
                acked.entry((actor.to_owned(), seq)).or_default().push(name);
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let doubled: Vec<_> = acked.iter().filter(|(_, nodes)| nodes.len() > 1).collect();
    assert!(doubled.is_empty(), "mail acknowledged twice: {doubled:?}");
    let keys: BTreeSet<(String, i64)> = acked.keys().cloned().collect();
    assert_eq!(keys, appended, "the acknowledged mail is the appended mail");
    let by_node = |name: &str| acked.values().filter(|nodes| nodes[0] == name).count();
    let (left, right) = (by_node("left"), by_node("right"));
    eprintln!(
        "contention: {} mails over {ACTORS} actors, left acknowledged {left}, right {right}",
        appended.len()
    );
    assert!(
        left > 0 && right > 0,
        "one node did all the work: {left} / {right}"
    );
    for node in &mut nodes {
        node.kill().await;
    }
}

const SWEEP: &str =
    "durable::node_wakes::tests::a_sweep_pass_in_another_process_holds_its_rows_until_it_dies";

/// A sweep pass is live for as long as its generation's lock beside the
/// database file is held, and every process on the file sees it: rows a pass
/// in another process condemned are held from this process's adoption while
/// that pass lives, and adopted once its process is killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sweep_pass_in_another_process_holds_its_rows_until_it_dies() {
    if let Some((role, database)) = child_role() {
        return run_child(&role, &database).await;
    }
    let tier = SqliteTier::open().await;
    let database = tier._dir.path().join("lash.db");
    let mut sweeper = Child::start(SWEEP, "sweeper", &database);
    assert_eq!(sweeper.next(Duration::from_secs(30)).await, "condemned");
    let store = tier.stores.session_store_factory();
    let pass = store
        .begin_attachment_sweep()
        .await
        .expect("begin this process's pass");
    let held = store
        .adopt_attachment_condemnations(&pass)
        .await
        .expect("adopt");
    assert_eq!(
        (held.held_by_live_pass, held.adopted.len()),
        (vec![swept()], 0),
        "a pass live in another process lost its row"
    );
    sweeper.kill().await;
    let adopted = store
        .adopt_attachment_condemnations(&pass)
        .await
        .expect("adopt");
    assert_eq!(
        adopted
            .adopted
            .iter()
            .map(|row| row.digest.clone())
            .collect::<Vec<_>>(),
        vec![swept()],
        "a killed process's pass kept its row"
    );
}

const GROUP_CUT: &str = "durable::node_wakes::tests::a_grouped_commit_killed_before_or_after_its_commit_is_all_or_nothing";

/// The owner commits the grouped-commit law queues into one batch.
const GROUPED: usize = 3;

fn grouped(index: usize) -> ActorKey {
    actor(&format!("grouped-{index}"))
}

/// Child `cut` over `database`: hold the writer after a heartbeat's fence,
/// queue `GROUPED` owner commits behind it so they form one batch, then let
/// it go. Reports each owner's revision, how many actor rows the batch's
/// `COMMIT` carries, and each commit's answer. Under `before` the connection
/// thread stops inside that `COMMIT`, before any of it is durable, until the
/// process is killed.
async fn run_grouped_child(cut: &str, database: &Path) {
    use std::future::Future as _;
    let pauses = crate::testing::SqlitePauses::default();
    let stores = SqliteStoreSet::open_with_options_and_clock(
        database,
        crate::SqliteStoreSetOptions {
            pauses: Some(pauses.clone()),
            ..crate::SqliteStoreSetOptions::standard(crate::SqliteSynchronous::Full)
        },
        Arc::new(lash_core::testing::TestClock::new(STORE_TIME)),
    )
    .await
    .expect("the child opens the database file");
    let store = Arc::new(stores.durable_store());
    let label = CommitLabel::new("law.grouped");
    let mut tx = MailTx::new();
    for index in 0..GROUPED {
        tx.create_actor(grouped(index), formats());
    }
    store
        .commit_mail(tx, label)
        .await
        .expect("create the actors");
    let node = store
        .register_node(&lash_durable::NodeSpec {
            node: NodeId::new("grouped"),
            decodes: vec![formats()],
            serves: Vec::new(),
            ttl_millis: 15_000,
        })
        .await
        .expect("register the owner");
    let claimed = store.claim(&node, GROUPED).await.expect("claim the actors");
    assert_eq!(claimed.len(), GROUPED, "every actor is claimed");
    let mut owners = Vec::new();
    for claim in &claimed {
        let owner = store
            .begin(&claim.actor, claim.epoch)
            .await
            .expect("open the owner");
        report(&format!("owns {} {}", claim.actor, owner.revision().0));
        owners.push(owner);
    }
    // The batch's COMMIT is the first that carries actor rows: the held
    // heartbeat's renews only its node.
    let touched = Arc::new(Mutex::new(BTreeSet::<i64>::new()));
    let stop = cut == "before";
    stores
        .process_env_store()
        .conn
        .call({
            let touched = Arc::clone(&touched);
            move |c| {
                let updated = Arc::clone(&touched);
                c.update_hook(Some(
                    move |_: rusqlite::hooks::Action, _: &str, table: &str, row: i64| {
                        if table == "actors" {
                            updated.lock().expect("touched rows").insert(row);
                        }
                    },
                ));
                let committing = Arc::clone(&touched);
                c.commit_hook(Some(move || {
                    let rows = std::mem::take(&mut *committing.lock().expect("touched rows"));
                    if !rows.is_empty() {
                        report(&format!("committing {}", rows.len()));
                        if stop {
                            loop {
                                std::thread::park();
                            }
                        }
                    }
                    false
                }));
                c.rollback_hook(Some(move || touched.lock().expect("touched rows").clear()));
                Ok(())
            }
        })
        .await
        .expect("watch the writer's commits");
    let hold = pauses.pause_after_fence();
    let holding = tokio::spawn({
        let store = Arc::clone(&store);
        let node = node.clone();
        async move { store.heartbeat(&node).await }
    });
    hold.wait_until_reached().await;
    let mut answers = Vec::new();
    for owner in owners {
        let actor = owner.actor().clone();
        let store = Arc::clone(&store);
        let mut commit = Box::pin(async move { store.commit(owner, label).await });
        std::future::poll_fn(|cx| {
            assert!(commit.as_mut().poll(cx).is_pending(), "the commit queues");
            std::task::Poll::Ready(())
        })
        .await;
        answers.push(tokio::spawn(async move {
            match commit.await {
                Ok(_) => report(&format!("answered {actor} ok")),
                Err(error) => report(&format!("answered {actor} err {error}")),
            }
        }));
    }
    hold.release();
    holding
        .await
        .expect("heartbeat task")
        .expect("the held heartbeat commits");
    for answer in answers {
        answer.await.expect("answer task");
    }
    std::future::pending::<()>().await;
}

/// FIG-5970: owner commits queued together form one write batch, and a
/// process killed around its shared `COMMIT` keeps all of them or none. A
/// child process groups `GROUPED` owner commits into one batch and is killed
/// with SIGKILL inside that `COMMIT`, before it is durable, or right after
/// the first commit is answered. On reopen no actor moved in the first cut;
/// in the second every actor moved, so every answered commit is present.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grouped_commit_killed_before_or_after_its_commit_is_all_or_nothing() {
    if let Some((cut, database)) = child_role() {
        return run_grouped_child(&cut, &database).await;
    }
    for cut in ["before", "after"] {
        let dir = tempfile::tempdir().expect("a database directory");
        let database = dir.path().join("lash.db");
        let mut child = Child::start(GROUP_CUT, cut, &database);
        let mut owned = BTreeMap::new();
        for _ in 0..GROUPED {
            let line = child.next(WATCHDOG).await;
            let (actor, revision) = line
                .strip_prefix("owns ")
                .and_then(|owns| owns.split_once(' '))
                .unwrap_or_else(|| panic!("an owner report: {line}"));
            owned.insert(
                actor.to_owned(),
                revision.parse::<i64>().expect("a revision"),
            );
        }
        assert_eq!(
            child.next(WATCHDOG).await,
            format!("committing {GROUPED}"),
            "{cut}: the owner commits share one COMMIT"
        );
        let mut answered = Vec::new();
        if cut == "after" {
            answered.push(child.next(WATCHDOG).await);
        }
        child.kill().await;
        tokio::time::timeout(WATCHDOG, async {
            while let Some(line) = child.reports.recv().await {
                answered.push(line);
            }
        })
        .await
        .expect("hang guard: the killed child's reports end");
        let stores = SqliteStoreSet::open(&database, crate::SqliteSynchronous::Full)
            .await
            .expect("reopen the killed child's database");
        let store = stores.durable_store();
        let mut moved = Vec::new();
        for index in 0..GROUPED {
            let actor = grouped(index);
            let revision = store
                .actor(&actor)
                .await
                .expect("read the actor")
                .expect("the actor exists")
                .revision
                .0;
            if revision != owned[&actor.to_string()] {
                moved.push(actor.to_string());
            }
        }
        if cut == "before" {
            assert!(moved.is_empty(), "an uncommitted batch left {moved:?}");
            assert!(answered.is_empty(), "answered before COMMIT: {answered:?}");
        } else {
            assert_eq!(
                moved.len(),
                GROUPED,
                "a committed batch kept only {moved:?}"
            );
            assert!(!answered.is_empty());
            for line in &answered {
                let actor = line
                    .strip_prefix("answered ")
                    .and_then(|answer| answer.strip_suffix(" ok"))
                    .unwrap_or_else(|| panic!("an Ok answer: {line}"));
                assert!(moved.iter().any(|moved| moved == actor), "{actor} lost");
            }
        }
    }
}

/// FIG-5954: same-file owner mail and its hint become visible together;
/// a listener never needs a second write transaction to discover the mail.
#[tokio::test]
async fn an_owned_mail_hint_is_already_committed_with_its_mail() {
    let dir = tempfile::tempdir().unwrap();
    let stores = SqliteStoreSet::open(dir.path().join("lash.db"), crate::SqliteSynchronous::Full)
        .await
        .unwrap();
    let store = stores.durable_store();
    let hot = actor("atomic-hint");
    let mut tx = MailTx::new();
    tx.create_actor(hot.clone(), formats());
    store
        .commit_mail(tx, CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    let lease = store
        .register_node(&lash_durable::NodeSpec {
            node: NodeId::new("hint-owner"),
            decodes: vec![formats()],
            serves: Vec::new(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    assert_eq!(store.claim(&lease, 1).await.unwrap().len(), 1);
    let mut tx = MailTx::new();
    tx.append(hot.clone(), MailKind::new("law.note"), "note");
    store
        .commit_mail(tx, CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    let node = lease.owner.node.as_str().to_owned();
    let payloads = store
        .conn
        .read(move |c| {
            c.prepare("SELECT actors FROM node_wakes WHERE node_id = ?1 ORDER BY seq")?
                .query_map([node], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap();
    assert!(
        payloads.iter().any(|payload| {
            node_wake_payload::decode(payload) == Some(NodeWakeEvent::Owned(vec![hot.clone()]))
        }),
        "the source commit owes no later wake transaction: {payloads:?}"
    );
    let before = payloads.len();
    let mut batch = WakeBatch::default();
    batch.owned.insert(lease.owner.node, [hot.clone()].into());
    stores
        .node_wakes()
        .unwrap()
        .committed(&batch)
        .await
        .unwrap();
    store
        .conn
        .write_flow(move |tx| {
            tx.execute(crate::durable::NODE_WAKE_ROLLBACK_MAIL, [hot.as_str()])?;
            Ok(crate::conn::TxOutcome::Rollback(()))
        })
        .await
        .unwrap();
    let after = store
        .conn
        .read(|c| {
            c.query_row("SELECT count(*) FROM node_wakes", [], |row| {
                row.get::<_, usize>(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "neither post-commit signalling nor rolled-back mail writes a hint"
    );
}
