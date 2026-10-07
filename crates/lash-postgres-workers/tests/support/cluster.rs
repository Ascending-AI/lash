//! A deployment under test: one PostgreSQL server, N node processes, the
//! witness ledger, and the store as an operator reads it.
//!
//! The test, not lash, starts the node processes
//! (`lash-postgres-workers-node`), kills them with SIGKILL, stops them on
//! stdin and holds their heartbeats. Each node's report lines are collected
//! in arrival order with the instant they arrived; its stderr is forwarded
//! to the test's.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_execution::{
    Backend, BackendParts, NoProjectionProviders, ProcessId, ProcessInput, ProcessProvenance,
    ProcessRecord, ProcessRegistration, StoreSet,
};
use lash_durable::{DurableStore, Notifier};
use lash_postgres_store::{PostgresStorage, PostgresStoreSet};
use lash_postgres_workers::events::{Command, Event, Report};
use lash_postgres_workers::node::{secrets, settings};
use lash_postgres_workers::process::WorkerEngine;
use lash_postgres_workers::witness::Hold;
use sqlx::postgres::PgPool;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Notify;

use super::postgres::Server;

const STEP_WAIT: Duration = Duration::from_secs(30);
const SECRET: &str = "lash-postgres-workers-completion-secret-0001";

/// One report line, as the test received it.
#[derive(Clone, Debug)]
pub struct Entry {
    /// When it arrived.
    pub at: Instant,
    /// The reporting node.
    pub node: String,
    /// What it reported.
    pub event: Event,
}

#[derive(Default)]
struct Reports {
    entries: Mutex<Vec<Entry>>,
    changed: Notify,
}

impl Reports {
    fn push(&self, entry: Entry) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry);
        self.changed.notify_waiters();
    }

    fn snapshot(&self) -> Vec<Entry> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// One node process.
struct Node {
    child: Child,
    stdin: Option<ChildStdin>,
}

/// One body entry or return, as the witness ledger holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effect {
    /// The call.
    pub call_id: String,
    /// The tool or step.
    pub tool: String,
    /// The node it ran on.
    pub node: String,
    /// `entered` or `returned`.
    pub phase: String,
}

/// One model attempt, as the witness ledger holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelAttempt {
    /// Which call of the turn.
    pub call_index: i32,
    /// Its attempt number.
    pub attempt: i32,
    /// The node that sent it.
    pub node: String,
}

/// The deployment.
pub struct Cluster {
    server: Server,
    nodes: BTreeMap<String, Node>,
    reports: Arc<Reports>,
    witness: PgPool,
    durable: Arc<dyn DurableStore>,
    backend: Backend,
    notifier: Notifier,
    hold: Hold,
}

impl Cluster {
    /// Start a server, provision it, and boot `nodes` with `notifier` and
    /// `hold`.
    pub async fn start(nodes: &[&str], notifier: Notifier, hold: Hold) -> Self {
        let server = Server::start().await;
        let witness = server.pool("lash_witness").await.expect("open the witness");
        let storage = PostgresStorage::connect(&server.url("lash", "lash"))
            .await
            .expect("open the lash store");
        let stores = PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
        );
        let durable = stores.durable_store();
        let backend = Backend::assemble(BackendParts {
            stores: Arc::new(stores),
            settings: settings(notifier),
            secrets: Some(secrets(SECRET).expect("the secret is long enough")),
            engines: vec![Arc::new(WorkerEngine)],
            providers: Arc::new(NoProjectionProviders),
        })
        .expect("the operator's backend assembles");
        let mut cluster = Self {
            server,
            nodes: BTreeMap::new(),
            reports: Arc::default(),
            witness,
            durable,
            backend,
            notifier,
            hold,
        };
        for node in nodes {
            cluster.boot(node);
        }
        for node in nodes {
            cluster
                .wait(
                    Duration::from_secs(60),
                    &format!("{node} registers"),
                    |entry| entry.node == *node && matches!(entry.event, Event::Registered { .. }),
                )
                .await;
        }
        cluster
    }

    /// Boot `name` as a node process.
    pub fn boot(&mut self, name: &str) {
        let notifier = match self.notifier {
            Notifier::AfterCommit => "after-commit",
            Notifier::PollOnly => "poll-only",
        };
        let mut child = tokio::process::Command::new(crate::NODE_BIN)
            .env("LASH_WORKERS_NODE", name)
            .env("LASH_WORKERS_DATABASE_URL", self.server.url("lash", "lash"))
            .env(
                "LASH_WORKERS_WITNESS_URL",
                self.server.url("lash_witness", "lash_witness_writer"),
            )
            .env("LASH_WORKERS_NOTIFIER", notifier)
            .env("LASH_WORKERS_HOLD", self.hold.name())
            .env("LASH_WORKERS_COMPLETION_SECRET", SECRET)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the node starts");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let reports = Arc::clone(&self.reports);
        let node = name.to_owned();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<Report>(&line) {
                    Ok(report) => {
                        eprintln!("[{node}] {line}");
                        reports.push(Entry {
                            at: Instant::now(),
                            node: report.node,
                            event: report.event,
                        });
                    }
                    Err(_) => eprintln!("[{node}] stdout: {line}"),
                }
            }
        });
        let node = name.to_owned();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{node}] {line}");
            }
        });
        let stdin = child.stdin.take();
        self.nodes.insert(name.to_owned(), Node { child, stdin });
    }

    /// Wait until `watcher`'s liveness probe has seen `node`'s lock held: a
    /// watcher reaps by lock only a boot it saw alive, so a node killed
    /// before any probe saw it is found by its lease instead.
    pub async fn seen_alive(&self, watcher: &str, node: &str) {
        if self.notifier != Notifier::AfterCommit {
            return;
        }
        self.wait(STEP_WAIT, &format!("{watcher} sees {node}'s liveness lock"), |entry| {
            entry.node == watcher
                && matches!(&entry.event, Event::Liveness { held, .. } if held.iter().any(|held| held == node))
        })
        .await;
    }

    /// Kill `name` with SIGKILL, record it in the witness ledger, and wait
    /// for the process to be gone. Returns when the kill was sent.
    pub async fn kill(&mut self, name: &str) -> Instant {
        let node = self.nodes.get_mut(name).expect("a booted node");
        node.child.start_kill().expect("the node is killed");
        let at = Instant::now();
        node.child.wait().await.expect("the killed node is reaped");
        self.nemesis("kill", Some(name)).await;
        at
    }

    /// Send `command` to `name`.
    pub async fn send(&mut self, name: &str, command: Command) {
        let node = self.nodes.get_mut(name).expect("a booted node");
        let stdin = node.stdin.as_mut().expect("the node's stdin is open");
        stdin
            .write_all(format!("{}\n", command.line()).as_bytes())
            .await
            .expect("the command reaches the node");
        stdin.flush().await.expect("the command is flushed");
    }

    /// Wait for `name` to exit on its own.
    pub async fn exited(&mut self, name: &str, within: Duration) {
        let node = self.nodes.get_mut(name).expect("a booted node");
        tokio::time::timeout(within, node.child.wait())
            .await
            .unwrap_or_else(|_| panic!("{name} did not exit within {within:?}"))
            .expect("the node is reaped");
    }

    /// Record a fault or marker in the witness ledger.
    pub async fn nemesis(&self, kind: &str, node: Option<&str>) {
        sqlx::query("INSERT INTO witness_nemesis (kind, node) VALUES ($1, $2)")
            .bind(kind)
            .bind(node)
            .execute(&self.witness)
            .await
            .expect("record the nemesis");
    }

    /// The server, to stop and restart it.
    pub fn server(&mut self) -> &mut Server {
        &mut self.server
    }

    /// Every body entry and return, in the order the ledger took them.
    pub async fn effects(&self) -> Vec<Effect> {
        let rows: Vec<(String, String, String, String)> =
            sqlx::query_as("SELECT call_id, tool, node, phase FROM witness_effects ORDER BY id")
                .fetch_all(&self.witness)
                .await
                .expect("read the effects");
        rows.into_iter()
            .map(|(call_id, tool, node, phase)| Effect {
                call_id,
                tool,
                node,
                phase,
            })
            .collect()
    }

    /// Every model attempt, in the order the ledger took them.
    pub async fn model_attempts(&self) -> Vec<ModelAttempt> {
        let rows: Vec<(i32, i32, String)> = sqlx::query_as(
            "SELECT call_index, attempt, node FROM witness_model_attempts ORDER BY id",
        )
        .fetch_all(&self.witness)
        .await
        .expect("read the model attempts");
        rows.into_iter()
            .map(|(call_index, attempt, node)| ModelAttempt {
                call_index,
                attempt,
                node,
            })
            .collect()
    }

    /// The store, read as an operator reads it: no fence, no node.
    pub fn durable(&self) -> &Arc<dyn DurableStore> {
        &self.durable
    }

    /// Admit the runbook's turn: what a producer outside the deployment
    /// commits.
    pub async fn admit_turn(&self) {
        lash_postgres_workers::turn::admit(&self.backend)
            .await
            .expect("the turn is admitted");
    }

    /// Register a runbook process that waits `wait` between its steps, under
    /// `start_key`.
    pub async fn register_process(&self, start_key: &str, wait: Duration) -> ProcessRecord {
        let registration = ProcessRegistration::new(
            ProcessInput::Engine {
                kind: lash_postgres_workers::process::KIND.to_owned(),
                payload: lash_postgres_workers::process::payload(wait),
            },
            ProcessProvenance::host(),
            lash_core_execution::LifetimeDecision::Detached,
        )
        .with_execution_env_ref(Some(
            lash_core_execution::testing::process_execution_env_fixture_ref(),
        ))
        .with_start_key(Some(lash_core_execution::StartKey::for_host(start_key)))
        .with_extra_event_types(
            lash_postgres_workers::process::declared_event_types().expect("the event types"),
        );
        self.backend
            .process_registry()
            .register_process(registration)
            .await
            .expect("the process registers")
    }

    /// The process registered under `start_key`, if one is retained.
    pub async fn process_by_start_key(&self, start_key: &str) -> Option<ProcessId> {
        let key = lash_core_execution::StartKey::for_host(start_key);
        self.backend
            .process_registry()
            .get_process_by_start_key(&key)
            .await
            .expect("read the start key")
            .map(|record| record.id)
    }

    /// A process's record.
    pub async fn process(&self, process: &ProcessId) -> ProcessRecord {
        self.backend
            .process_registry()
            .get_process(process)
            .await
            .expect("read the process")
            .expect("the process is retained")
    }

    /// The types of a process's events, in sequence order.
    pub async fn event_types(&self, process: &ProcessId) -> Vec<String> {
        self.backend
            .process_registry()
            .recent_events(process, 100)
            .await
            .expect("read the process's events")
            .into_iter()
            .map(|event| event.event_type)
            .collect()
    }

    /// Every report so far, in arrival order.
    pub fn reports(&self) -> Vec<Entry> {
        self.reports.snapshot()
    }

    /// Wait up to `within` for the first report `matches` accepts, already
    /// received or not.
    pub async fn wait(
        &self,
        within: Duration,
        what: &str,
        matches: impl Fn(&Entry) -> bool,
    ) -> Entry {
        self.wait_after(None, within, what, matches).await
    }

    /// Wait up to `within` for the first report after `after` (an instant)
    /// that `matches` accepts.
    pub async fn wait_after(
        &self,
        after: Option<Instant>,
        within: Duration,
        what: &str,
        matches: impl Fn(&Entry) -> bool,
    ) -> Entry {
        let deadline = Instant::now() + within;
        loop {
            let changed = self.reports.changed.notified();
            if let Some(entry) = self
                .reports
                .snapshot()
                .into_iter()
                .find(|entry| after.is_none_or(|after| entry.at >= after) && matches(entry))
            {
                return entry;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, changed).await.is_err() {
                panic!("timed out after {within:?} waiting for {what}");
            }
        }
    }
}

/// Poll `check` every 20 ms for up to `within` until it answers `Some`.
pub async fn until<T, F, Fut>(within: Duration, what: &str, check: F) -> T
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + within;
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {within:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
