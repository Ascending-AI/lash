//! Laws of node wakes over a SQLite database file (FIG-5422): the node-wake
//! laws every dialect keeps (`lash_durable::laws::node_wakes`), each node on
//! store handles of its own over the one file; and the laws only separate OS
//! processes can show, run by this test binary re-executing itself as a
//! node: a wake crosses processes, a killed process's lock
//! is released at once, processes writing one file at once lose and double
//! no work, and a sweep pass's liveness lock is seen from every process.

// Test code: the laws spawn this test binary and read its environment.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_execution::StoreSet as _;
use lash_durable::laws::node_wakes::{NodeWakeTier, serve_node};
use lash_durable::runner::{Activation, Exit, Owned, Runner, RunnerConfig};
use lash_durable::{
    ActorKey, BootLiveness, CommitLabel, DurableError, DurableSettings, DurableStore, FormatSet,
    LeaseSettings, MailKind, MailTx, NodeId, NodeLease, NodeWakeEvent, NodeWakeFeed, NodeWakes,
    Owner, Reaped, WakeBatch,
};
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::mpsc;

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
    mail_whose_hint_is_lost_reaches_a_hot_owner_within_its_poll,
    a_dead_node_is_reaped_through_its_lock_long_before_its_lease_lapses,
    a_readied_actor_is_claimed_by_one_attempt_on_one_node,
    a_hint_to_a_dead_node_is_backed_by_the_claim_poll,
);

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
                .stdin(Stdio::null())
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
/// so the next mail readies it for whichever node claims first.
struct Tally;

#[async_trait::async_trait]
impl Activation for Tally {
    async fn activate(&self, owned: Owned) -> Exit {
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
fn swept() -> lash_core_execution::AttachmentId {
    lash_core_execution::AttachmentId::parse("a1".repeat(32)).expect("an attachment id")
}

/// Facts observed at the production runner's node-wake boundary.
#[derive(Debug)]
enum WakeEvidence {
    Watched(Vec<BootLiveness>),
    Woke(Vec<ActorKey>),
    Reaped(Vec<Reaped>),
}

/// Forwards every operation unchanged and records what the runner actually
/// observed, rather than guessing its progress from elapsed wall time.
struct ObservedWakes {
    inner: Arc<dyn NodeWakes>,
    evidence: mpsc::UnboundedSender<WakeEvidence>,
}

struct ObservedFeed {
    inner: Box<dyn NodeWakeFeed>,
    evidence: mpsc::UnboundedSender<WakeEvidence>,
}

#[async_trait::async_trait]
impl NodeWakeFeed for ObservedFeed {
    async fn next(&mut self) -> NodeWakeEvent {
        let event = self.inner.next().await;
        if let NodeWakeEvent::Owned(actors) = &event {
            let _ = self.evidence.send(WakeEvidence::Woke(actors.clone()));
        }
        event
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

    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        Ok(Box::new(ObservedFeed {
            inner: self.inner.listen(lease).await?,
            evidence: self.evidence.clone(),
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
    reached: impl Fn(&WakeEvidence) -> bool,
) -> WakeEvidence {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let event = evidence
                .recv()
                .await
                .expect("the runner keeps its evidence channel open");
            if reached(&event) {
                return event;
            }
        }
    })
    .await
    .expect("hang guard: the runner never produced the required node-wake evidence")
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
        _ => (
            Arc::new(Tally),
            DurableSettings {
                max_active: 4,
                claim_batch: 4,
                ..DurableSettings::default()
            },
        ),
    };
    let config = settings.validate().expect("the child's settings validate");
    let mut runner = Runner::new(
        Arc::new(stores.durable_store()),
        Arc::new(lash_core_execution::runtime::SystemClock),
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

/// Three processes write one database file at once: this one appends mail
/// to many actors while two child nodes claim them, acknowledge each one's
/// mail and release it, all under SQLite's one writer and its busy timeout.
/// Every mail is acknowledged exactly once, by one node, and both nodes did
/// part of the work.
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
    for node in &mut nodes {
        assert_eq!(node.next(Duration::from_secs(30)).await, "serving");
    }
    let mut appended = BTreeSet::new();
    for _ in 0..ROUNDS {
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

/// FIG-5555: wake hints preserve delimiter-bearing identities and the
/// largest plain key that fits the transport envelope, without oversized payloads.
#[test]
fn wake_payloads_round_trip_delimiters_and_a_maximal_fitting_key() {
    for actors in [
        std::collections::BTreeSet::from([
            ActorKey::process("b\ns/a").expect("newlines are valid"),
            ActorKey::session("quotes\"\\\0雪").expect("escaped text is valid"),
        ]),
        std::collections::BTreeSet::from([ActorKey::session(
            &"x".repeat(node_wake_payload::MAX_BYTES - 6),
        )
        .expect("actor ids have no length bound")]),
        std::collections::BTreeSet::from([ActorKey::session(
            &"\n".repeat((node_wake_payload::MAX_BYTES - 6) / 2),
        )
        .expect("escaping counts toward the bound")]),
    ] {
        let batch = WakeBatch {
            owned: std::collections::BTreeMap::from([(NodeId::new("round-trip"), actors.clone())]),
            ..WakeBatch::default()
        };
        let payloads: Vec<String> = rows(&batch)
            .into_iter()
            .map(|(_, payload)| payload)
            .collect();
        assert!(
            payloads
                .iter()
                .all(|payload| payload.len() <= node_wake_payload::MAX_BYTES)
        );
        let carried: std::collections::BTreeSet<ActorKey> = payloads
            .iter()
            .flat_map(|payload| {
                match node_wake_payload::decode(payload).expect("a valid wake payload") {
                    NodeWakeEvent::Owned(actors) => actors,
                    event => panic!("expected owned actors, got {event:?}"),
                }
            })
            .collect();
        assert_eq!(
            carried, actors,
            "wake hints preserve every complete actor identity"
        );
    }
}

/// FIG-5555: both transports split encoded batches at the byte bound and
/// replace individually oversized keys with one typed store-scan hint.
#[test]
fn wake_payloads_split_encoded_batches_and_poll_for_oversized_keys() {
    let actors: std::collections::BTreeSet<ActorKey> = (0..1_000)
        .map(|index| {
            ActorKey::session(&format!("crowded-{index:04}\n\0雪"))
                .expect("a valid escaped actor key")
        })
        .collect();
    let mut oversized = actors.clone();
    oversized.extend([
        ActorKey::session(&"x".repeat(node_wake_payload::MAX_BYTES)).expect("unbounded ids"),
        ActorKey::process(&"\n".repeat(node_wake_payload::MAX_BYTES / 2)).expect("unbounded ids"),
    ]);
    let batch = WakeBatch {
        ready: std::collections::BTreeSet::from([NodeId::new("ready")]),
        owned: std::collections::BTreeMap::from([(NodeId::new("owned"), oversized)]),
        ..WakeBatch::default()
    };
    let payloads: Vec<String> = rows(&batch)
        .into_iter()
        .map(|(_, payload)| payload)
        .collect();
    assert!(payloads.len() > 3, "the encoded crowd must split");
    let mut carried = std::collections::BTreeSet::new();
    let mut ready = 0;
    let mut poll_store = 0;
    for payload in payloads {
        assert!(payload.len() <= node_wake_payload::MAX_BYTES);
        assert!(
            !payload.contains('\0'),
            "NOTIFY payloads contain no raw NUL"
        );
        match node_wake_payload::decode(&payload).expect("a bounded wake envelope") {
            NodeWakeEvent::Ready => ready += 1,
            NodeWakeEvent::Owned(actors) => carried.extend(actors),
            NodeWakeEvent::PollStore => poll_store += 1,
            event => panic!("unexpected wake event: {event:?}"),
        }
    }
    assert_eq!(ready, 1);
    assert_eq!(poll_store, 1, "oversized keys coalesce into one store scan");
    assert_eq!(
        carried, actors,
        "every fitting key rides a complete envelope"
    );
}
