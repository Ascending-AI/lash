//! Laws of node wakes over a SQLite database file (FIG-5422): the node-wake
//! laws every dialect keeps (`lash_durable::laws::node_wakes`), each node on
//! store handles of its own over the one file; and the laws only separate OS
//! processes can show, run by this test binary re-executing itself as a
//! node: a wake crosses processes in about a poll, a killed process's lock
//! is released at once, processes writing one file at once lose and double
//! no work, and a sweep pass's liveness lock is seen from every process.

// Test code: the laws spawn this test binary and read its environment.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lash_core_execution::StoreSet as _;
use lash_durable::laws::node_wakes::{NodeWakeTier, serve_node};
use lash_durable::runner::{Activation, Exit, Owned, Runner, RunnerConfig};
use lash_durable::{
    CommitLabel, DurableSettings, DurableStore, FormatSet, LeaseSettings, MailKind, MailTx, NodeId,
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
        let stores = SqliteStoreSet::open(dir.path().join("lash.db"))
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
    mail_from_another_node_reaches_a_hot_owner_through_its_hint,
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

fn wall_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is past the epoch")
        .as_micros()
}

fn report(line: &str) {
    println!("{REPORT}{line}");
}

/// The role this process plays, when it was started as a child node.
fn child_role() -> Option<(String, PathBuf)> {
    let value = std::env::var(CHILD).ok()?;
    let (role, path) = value.split_once('\n')?;
    Some((role.to_owned(), PathBuf::from(path)))
}

/// One child node process, and the report lines it prints.
struct Child {
    process: tokio::process::Child,
    reports: mpsc::UnboundedReceiver<String>,
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
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("the child node starts");
        let stdout = process.stdout.take().expect("piped stdout");
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
        Self { process, reports }
    }

    /// The next report line, within `within`.
    async fn next(&mut self, within: Duration) -> String {
        tokio::time::timeout(within, self.reports.recv())
            .await
            .expect("the child reports in time")
            .expect("the child is running")
    }

    /// Kill the child with SIGKILL and wait for it to be gone.
    async fn kill(&mut self) {
        self.process.start_kill().expect("the child is killed");
        self.process
            .wait()
            .await
            .expect("the killed child is reaped");
    }
}

/// Holds every actor it claims hot and reports each mail it acknowledged,
/// with the wall-clock instant it read it.
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
                let read = wall_micros();
                let seqs: Vec<i64> = tx.mail().iter().map(|mail| mail.seq.0).collect();
                tx.ack_seen();
                if owned.commit(tx, CommitLabel::new("law.ack")).await.is_err() {
                    return Exit::Released;
                }
                for seq in seqs {
                    report(&format!("acked {} {seq} {read}", owned.actor()));
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
    lash_core_execution::AttachmentId::parse("swept-elsewhere").expect("an attachment id")
}

/// Serve child node `role` over `database` until killed; the `sweeper`
/// instead holds a sweep pass that condemned one attachment.
async fn run_child(role: &str, database: &Path) {
    let stores = SqliteStoreSet::open(database)
        .await
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
    let node_wakes = stores.node_wakes();
    let (activation, settings): (Arc<dyn Activation>, DurableSettings) = match role {
        // A slow mail poll: mail that reaches it sooner came by its hint.
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

/// The child's CPU time so far, from `/proc`, on Linux.
fn cpu_time(pid: u32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    // The kernel's USER_HZ is 100 on every Linux lash runs on.
    Some(Duration::from_millis(ticks * 10))
}

const CROSS_PROCESS: &str = "durable::node_wakes::tests::a_wake_crosses_processes_in_about_a_poll_and_a_killed_process_is_reaped_by_its_lock";

/// Two processes serve one database file as two nodes. Mail the parent's
/// node writes to an actor hot on the child's node reaches it through the
/// wake row in about one listener poll, far inside the child's ten-second
/// mail poll; an idle child costs little CPU; and when the child is killed
/// with SIGKILL the kernel drops its liveness lock, so the parent's node
/// reaps it and takes its actor over in a small fraction of the
/// fifteen-second lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wake_crosses_processes_in_about_a_poll_and_a_killed_process_is_reaped_by_its_lock() {
    if let Some((role, database)) = child_role() {
        return run_child(&role, &database).await;
    }
    const WAKES: usize = 20;
    let tier = SqliteTier::open().await;
    let database = tier._dir.path().join("lash.db");
    let store = tier.stores.durable_store();
    let hot = actor("hot");
    let mut tx = MailTx::new();
    tx.create_actor(hot.clone(), formats());
    store
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await
        .expect("create the actor");
    let mut child = Child::start(CROSS_PROCESS, "hot", &database);
    assert_eq!(child.next(Duration::from_secs(30)).await, "serving");
    assert_eq!(
        child.next(Duration::from_secs(10)).await,
        format!("owns {hot}")
    );
    let parent = serve_node(
        Arc::new(store.clone()),
        tier.stores.node_wakes(),
        "parent",
        DurableSettings::default(),
    );
    let node_wakes = tier.stores.node_wakes().expect("node wakes");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !node_wakes
        .liveness()
        .await
        .expect("probe")
        .iter()
        .any(|liveness| liveness.boot.node.as_str() == "parent" && liveness.held)
    {
        assert!(Instant::now() < deadline, "the parent's node listens");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let mut latencies = Vec::with_capacity(WAKES);
    for _ in 0..WAKES {
        let mut tx = MailTx::new();
        tx.append(hot.clone(), MailKind::new("law.note"), "note");
        let sent = wall_micros();
        let commit = store
            .commit_mail(tx, CommitLabel::MAIL_SESSION)
            .await
            .expect("mail the hot actor");
        parent.hints.woke(&commit);
        let acked = child.next(Duration::from_secs(5)).await;
        let read: u128 = acked
            .rsplit(' ')
            .next()
            .and_then(|read| read.parse().ok())
            .unwrap_or_else(|| panic!("an ack report: {acked}"));
        latencies.push(Duration::from_micros(
            u64::try_from(read.saturating_sub(sent)).unwrap_or(u64::MAX),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    latencies.sort();
    let p50 = latencies[WAKES / 2];
    let max = latencies[WAKES - 1];
    eprintln!(
        "cross-process wake over {WAKES} mails: p50 {p50:?}, max {max:?}, listener poll {POLL:?}"
    );
    assert!(
        max < Duration::from_secs(1),
        "a cross-process wake took {max:?}, as long as a poll: {latencies:?}"
    );

    let pid = child.process.id().expect("the child's pid");
    if let Some(before) = cpu_time(pid) {
        let idle = Duration::from_secs(5);
        tokio::time::sleep(idle).await;
        if let Some(after) = cpu_time(pid) {
            let spent = after.saturating_sub(before);
            eprintln!(
                "an idle child node spent {spent:?} of CPU in {idle:?} ({:.2}% of a core)",
                spent.as_secs_f64() * 100.0 / idle.as_secs_f64()
            );
        }
    }

    // Let the parent's watch see the child's lock held.
    tokio::time::sleep(Duration::from_millis(600)).await;
    child.kill().await;
    let killed = Instant::now();
    loop {
        let owner = store
            .actor(&hot)
            .await
            .expect("read the actor")
            .and_then(|snapshot| snapshot.owner)
            .map(|owner| owner.node.as_str().to_owned());
        if owner.as_deref() == Some("parent") {
            break;
        }
        assert!(
            killed.elapsed() < Duration::from_secs(10),
            "the parent never took the killed child's actor over"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let failover = killed.elapsed();
    eprintln!("cross-process lock failover: the parent owns the actor {failover:?} after SIGKILL");
    assert!(
        failover < Duration::from_secs(3),
        "the takeover took {failover:?}, near the lease"
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
