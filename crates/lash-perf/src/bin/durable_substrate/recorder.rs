//! What the bench observes of the production runtime: a store-set
//! decorator whose durable store forwards every call unchanged and records
//! each write transaction's label, latency and encoded commit (the graph
//! nodes it stages, by kind, and its domain writes), and the shared recorder
//! the scripted model and engine report to.
//!
//! The runner, the activations and the fences are the production ones; the
//! decorator only watches. Its own cost is one mutex push per transaction.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_core_execution::AttachmentStore;
use lash_core_execution::Clock;
use lash_durable::domain::pages::{
    CandidatePublication, CandidateReceipt, PageStore, RetirementBacklog, RetirementBudget,
    RetirementProgress, RootLease, RootLeaseId, RootLeaseRequest, StoredPage,
};
use lash_durable::domain::{
    ExecKey, ExecRootRow, HydratedGraph, OwnerKey, ProcessActorRow, RootOwner, RunFacts, ScopeKey,
    SessionCloseRow, TurnEnd, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx,
    NodeLease, NodeSpec, NodeWakes, Reaped,
};
use lash_execution_contracts::backend::StoreBindingId;
use lash_execution_contracts::backend::StoreSet;
use lash_execution_contracts::deployment::DeploymentStore;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
use lash_execution_contracts::process::ProcessEnvStore;
use lash_execution_contracts::process::ProcessRegistry;
use lash_protocol_types::sync::MutexExt as _;
use lash_protocol_types::{ProcessId, SessionId, TurnId};
use serde::Serialize;
use tokio::sync::oneshot;

/// One write transaction through the durable port.
#[derive(Clone, Debug, Serialize)]
pub struct Transaction {
    /// Microseconds since the recorder started, at the call.
    pub at_us: u64,
    /// The actor it wrote, or the node for lease calls.
    pub actor: String,
    /// Its commit label, or the port method for unlabelled calls.
    pub label: &'static str,
    /// Its latency, microseconds.
    pub micros: u64,
    /// Whether the store accepted it.
    pub ok: bool,
    /// Encoded bytes of the turn checkpoint nodes it staged: PhaseRoots and
    /// the TurnContent they name.
    pub checkpoint_bytes: usize,
    /// Encoded bytes of every other node it staged: execution state, cell
    /// and process roots, reports and facts.
    pub snapshot_bytes: usize,
    /// How many graph nodes it staged.
    pub graph_nodes: usize,
    /// Encoded bytes of every node it staged.
    pub graph_bytes: usize,
    /// Actual new canonical node bytes inserted by the store, zero on refusal.
    pub installed_bytes: usize,
    /// Physical SQL in this owning commit, including its actor fence.
    pub sql: Option<lash_core_execution::facade_support::sql::Receipt>,
    /// The nodes it staged by kind, keyed by the kind's number.
    pub by_kind: BTreeMap<u32, KindBytes>,
    /// How many execution roots it put: a cell's or process's park.
    pub exec_roots: usize,
    /// How many domain writes it carried.
    pub domain_writes: usize,
    /// Run facts appended by this accepted owner transaction, by stored kind.
    pub run_facts: BTreeMap<String, usize>,
}

/// One kind's share of a commit's staged nodes.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct KindBytes {
    /// How many nodes of the kind it staged.
    pub nodes: usize,
    /// Their encoded bytes.
    pub bytes: usize,
}

/// What one commit encodes: the nodes it stages and its domain writes.
#[derive(Clone, Debug, Default)]
struct Encoded {
    checkpoint_bytes: usize,
    snapshot_bytes: usize,
    graph_nodes: usize,
    graph_bytes: usize,
    by_kind: BTreeMap<u32, KindBytes>,
    exec_roots: usize,
    domain_writes: usize,
    run_facts: BTreeMap<String, usize>,
    /// The reason of the park it commits, if it parks its actor.
    parked: Option<String>,
    sql: Option<lash_core_execution::facade_support::sql::Receipt>,
}

impl Encoded {
    fn of(tx: &ActorTx) -> Self {
        use lash_core_store::checkpoint_graph::NodeKind;
        let mut encoded = Self {
            exec_roots: tx
                .domain()
                .iter()
                .filter(|write| {
                    matches!(
                        write,
                        lash_durable::DomainWrite::ExecRoot(
                            lash_durable::domain::graph::ExecRootWrite::Put { .. }
                        )
                    )
                })
                .count(),
            domain_writes: tx.domain().len(),
            parked: tx.domain().iter().find_map(|write| match write {
                lash_durable::DomainWrite::ActorPark { reason_json } => reason_json.clone(),
                _ => None,
            }),
            ..Self::default()
        };
        for write in tx.domain() {
            if let lash_durable::DomainWrite::RunFact(
                lash_durable::domain::RunFactWrite::Append { kind, .. },
            ) = write
            {
                *encoded
                    .run_facts
                    .entry(kind.as_str().to_owned())
                    .or_default() += 1;
            }
        }
        for node in tx.graph().values() {
            let bytes = node.bytes.len();
            // A node whose envelope does not read is refused by the commit;
            // it still counts toward what the commit carried.
            let kind = node.envelope().map_or(0, |envelope| envelope.kind.0);
            let share = encoded.by_kind.entry(kind).or_default();
            share.nodes += 1;
            share.bytes += bytes;
            encoded.graph_nodes += 1;
            encoded.graph_bytes += bytes;
            if kind == NodeKind::PHASE_ROOT.0 || kind == NodeKind::TURN_CONTENT.0 {
                encoded.checkpoint_bytes += bytes;
            } else {
                encoded.snapshot_bytes += bytes;
            }
        }
        encoded
    }
}

type Waiters = HashMap<(String, &'static str), oneshot::Sender<Instant>>;

/// What every node and every scripted component reports to.
pub struct Recorder {
    start: Instant,
    transactions: Mutex<Vec<Transaction>>,
    claims: Mutex<Vec<(Instant, String, String)>>,
    model_calls: Mutex<HashMap<String, Vec<Instant>>>,
    waiters: Mutex<Waiters>,
    parks: Mutex<HashMap<String, oneshot::Sender<String>>>,
    pub process_counters: Arc<crate::process_counters::Counters>,
    changed: tokio::sync::Notify,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            transactions: Mutex::default(),
            claims: Mutex::default(),
            model_calls: Mutex::default(),
            waiters: Mutex::default(),
            parks: Mutex::default(),
            process_counters: Arc::default(),
            changed: tokio::sync::Notify::new(),
        }
    }
}

impl Recorder {
    pub fn since(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.start).as_micros()).unwrap_or(u64::MAX)
    }

    pub fn port_calls(&self) -> Vec<crate::process_counters::Call> {
        self.process_counters.calls()
    }
    pub fn discoveries(&self) -> Vec<crate::process_counters::Discovery> {
        self.process_counters.discoveries()
    }
    // Idle nodes register but make no claim writes (FIG-7044). The wrapper
    // records registration only after the store has completed it.
    pub async fn ready(&self, nodes: std::ops::Range<usize>) -> anyhow::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if nodes.clone().all(|n| {
                    self.transactions.lock_recover().iter().any(|t| {
                        t.actor == format!("node-{n}") && t.label == "node.register" && t.ok
                    })
                }) {
                    break;
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("nodes did not complete registration"))
    }

    /// Microseconds since the recorder started, now.
    pub fn now_us(&self) -> u64 {
        self.since(Instant::now())
    }

    /// Resolve with the instant `actor` commits under `label`.
    pub fn watch(&self, actor: &ActorKey, label: CommitLabel) -> oneshot::Receiver<Instant> {
        let (send, receive) = oneshot::channel();
        self.waiters
            .lock_recover()
            .insert((actor.to_string(), label.as_str()), send);
        receive
    }

    /// A newly minted process may end before registration is acknowledged.
    /// Hold the transaction lock through waiter admission so that a commit
    /// either satisfies this read or sees the waiter it must notify.
    pub fn watch_terminal(&self, actor: &ActorKey) -> oneshot::Receiver<Instant> {
        let (send, receive) = oneshot::channel();
        let transactions = self.transactions.lock_recover();
        if let Some(done) = transactions.iter().find(|tx| {
            tx.actor == actor.to_string()
                && tx.label == CommitLabel::PROCESS_TERMINAL.as_str()
                && tx.ok
        }) {
            let _ =
                send.send(self.start + std::time::Duration::from_micros(done.at_us + done.micros));
        } else {
            self.waiters.lock_recover().insert(
                (actor.to_string(), CommitLabel::PROCESS_TERMINAL.as_str()),
                send,
            );
        }
        receive
    }

    /// Resolve with the reason `actor` parks with, when it commits a park.
    pub fn watch_park(&self, actor: &ActorKey) -> oneshot::Receiver<String> {
        let (send, receive) = oneshot::channel();
        self.parks.lock_recover().insert(actor.to_string(), send);
        receive
    }

    /// The scripted model was entered for `session`.
    pub fn model_call(&self, session: &SessionId) {
        self.model_calls
            .lock_recover()
            .entry(session.to_string())
            .or_default()
            .push(Instant::now());
    }

    /// Every model entry for `session`, in order.
    pub fn model_calls(&self, session: &SessionId) -> Vec<Instant> {
        self.model_calls
            .lock_recover()
            .get(session.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// The transactions recorded from `from_us` on.
    pub fn transactions_since(&self, from_us: u64) -> Vec<Transaction> {
        self.transactions
            .lock_recover()
            .iter()
            .filter(|transaction| transaction.at_us >= from_us)
            .cloned()
            .collect()
    }

    /// The claims of `actor`, as (instant, node).
    pub fn claims_of(&self, actor: &ActorKey) -> Vec<(Instant, String)> {
        let actor = actor.to_string();
        self.claims
            .lock_recover()
            .iter()
            .filter(|(_, claimed, _)| *claimed == actor)
            .map(|(at, _, node)| (*at, node.clone()))
            .collect()
    }

    fn record(&self, started: Instant, actor: String, label: &'static str, ok: bool) {
        self.record_with(started, actor, label, ok, Encoded::default());
    }

    fn record_with(
        &self,
        started: Instant,
        actor: String,
        label: &'static str,
        ok: bool,
        encoded: Encoded,
    ) {
        let ended = Instant::now();
        let installed_bytes = if ok {
            encoded
                .sql
                .as_ref()
                .map_or(0, |sql| sql.newly_installed_graph_bytes as usize)
        } else {
            0
        };
        let transaction = Transaction {
            at_us: self.since(started),
            micros: u64::try_from(ended.saturating_duration_since(started).as_micros())
                .unwrap_or(u64::MAX),
            actor,
            label,
            ok,
            checkpoint_bytes: encoded.checkpoint_bytes,
            snapshot_bytes: encoded.snapshot_bytes,
            graph_nodes: encoded.graph_nodes,
            graph_bytes: encoded.graph_bytes,
            installed_bytes,
            sql: encoded.sql,
            by_kind: encoded.by_kind,
            exec_roots: encoded.exec_roots,
            domain_writes: encoded.domain_writes,
            run_facts: encoded.run_facts,
        };
        let key = (transaction.actor.clone(), label);
        let park_actor = transaction.actor.clone();
        self.transactions.lock_recover().push(transaction);
        self.changed.notify_waiters();
        // Publishing the completion must make its receipt visible first.
        if ok && let Some(waiter) = self.waiters.lock_recover().remove(&key) {
            let _ = waiter.send(ended);
        }
        if ok
            && let Some(reason) = encoded.parked
            && let Some(waiter) = self.parks.lock_recover().remove(&park_actor)
        {
            let _ = waiter.send(reason);
        }
    }
}

/// A durable store that records each write transaction, a candidate's
/// page publication included.
#[derive(Clone)]
pub struct RecordingStore {
    retirement_batches: Arc<AtomicU64>,
    inner: Arc<dyn DurableStore>,
    node: String,
    recorder: Arc<Recorder>,
    dialect: &'static str,
}

#[async_trait::async_trait]
impl DurableStore for RecordingStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.recorder.process_counters.record("now");
        self.inner.now().await
    }

    fn pages(&self) -> Option<std::sync::Arc<dyn lash_durable::domain::pages::PageStore>> {
        self.inner.pages()?;
        Some(Arc::new(self.clone()))
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        let started = Instant::now();
        let answer = self.inner.register_node(spec).await;
        self.recorder
            .record(started, self.node.clone(), "node.register", answer.is_ok());
        answer
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        let started = Instant::now();
        let answer = self.inner.heartbeat(node).await;
        self.recorder
            .record(started, self.node.clone(), "node.heartbeat", answer.is_ok());
        answer
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        let started = Instant::now();
        let answer = self.inner.reap(reaper).await;
        self.recorder
            .record(started, self.node.clone(), "node.reap", answer.is_ok());
        answer
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        let started = Instant::now();
        let answer = self.inner.release_node(node).await;
        self.recorder
            .record(started, self.node.clone(), "node.release", answer.is_ok());
        answer
    }

    async fn acquire_idle_for_testing(
        &self,
        node: &lash_durable::NodeLease,
        actor: &lash_durable::ActorKey,
        epoch: lash_durable::Epoch,
    ) -> Result<lash_durable::Epoch, lash_durable::DurableError> {
        self.inner
            .acquire_idle_for_testing(node, actor, epoch)
            .await
    }

    async fn readiness(&self) -> Result<lash_durable::Readiness, DurableError> {
        self.inner.readiness().await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        let started = Instant::now();
        let answer = self.inner.claim(node, limit).await;
        if let Ok(claimed) = &answer {
            let at = Instant::now();
            let mut claims = self.recorder.claims.lock_recover();
            for claimed in claimed {
                claims.push((at, claimed.actor.to_string(), self.node.clone()));
            }
        }
        self.recorder
            .record(started, self.node.clone(), "node.claim", answer.is_ok());
        answer
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        let started = Instant::now();
        let answer = self.inner.mark_draining(node).await;
        self.recorder
            .record(started, self.node.clone(), "node.drain", answer.is_ok());
        answer
    }

    async fn live_nodes(&self) -> Result<Vec<lash_durable::NodeCapability>, DurableError> {
        self.inner.live_nodes().await
    }

    async fn actor_requirements(
        &self,
        after: Option<&lash_durable::ActorKey>,
        limit: usize,
    ) -> Result<Vec<(lash_durable::ActorKey, Vec<String>)>, DurableError> {
        self.inner.actor_requirements(after, limit).await
    }

    async fn install_packages(
        &self,
        artifacts: &[lash_durable::packages::ArtifactInstall],
        sets: &[lash_durable::packages::SetInstall],
    ) -> Result<(), DurableError> {
        self.inner.install_packages(artifacts, sets).await
    }

    async fn package_catalog(&self) -> Result<lash_durable::PackageCatalog, DurableError> {
        self.inner.package_catalog().await
    }

    async fn package_surface(
        &self,
        digest: &lash_durable::PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, DurableError> {
        self.inner.package_surface(digest).await
    }

    async fn package_paths(
        &self,
        functions: &[lash_durable::packages::FunctionId],
    ) -> Result<
        std::collections::BTreeMap<
            lash_durable::packages::FunctionId,
            std::collections::BTreeSet<String>,
        >,
        DurableError,
    > {
        self.inner.package_paths(functions).await
    }

    async fn internal_operations(
        &self,
        operations: &[lash_durable::packages::EffectName],
    ) -> Result<
        std::collections::BTreeMap<
            lash_durable::packages::EffectName,
            std::collections::BTreeSet<String>,
        >,
        DurableError,
    > {
        self.inner.internal_operations(operations).await
    }

    async fn package_labels(
        &self,
        artifacts: &[lash_durable::ArtifactDigest],
    ) -> Result<std::collections::BTreeMap<lash_durable::ArtifactDigest, Vec<String>>, DurableError>
    {
        self.inner.package_labels(artifacts).await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.recorder.process_counters.record("begin");
        self.inner.begin(actor, epoch).await
    }

    async fn begin_retained(
        &self,
        actor: &ActorKey,
        epoch: lash_durable::Epoch,
        expected: lash_durable::StateRevision,
        max_mail_bytes: u64,
    ) -> Result<ActorTx, DurableError> {
        self.recorder.process_counters.record("begin");
        self.inner
            .begin_retained(actor, epoch, expected, max_mail_bytes)
            .await
    }

    async fn open_process(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<(ActorTx, lash_durable::domain::ProcessView), DurableError> {
        self.recorder.process_counters.record("open_process");
        self.inner.open_process(actor, epoch).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let actor = tx.actor().to_string();
        let mut encoded = Encoded::of(&tx);
        let started = Instant::now();
        let (answer, sql) = lash_core_ids::operational_metrics::sql::with_owner(
            actor.clone(),
            tx.epoch().0,
            tx.revision().0,
            lash_core_ids::operational_metrics::sql::collect(
                format!("{actor}/{}", label.as_str()),
                self.dialect,
                self.inner.commit(tx, label),
            ),
        )
        .await;
        encoded.sql = Some(sql);
        self.recorder
            .record_with(started, actor, label.as_str(), answer.is_ok(), encoded);
        answer
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let mut encoded = Encoded {
            domain_writes: tx.writes().len(),
            ..Encoded::default()
        };
        let started = Instant::now();
        let (answer, sql) = lash_core_ids::operational_metrics::sql::collect(
            format!("{}/{}", self.node, label.as_str()),
            self.dialect,
            self.inner.commit_mail(tx, label),
        )
        .await;
        encoded.sql = Some(sql);
        self.recorder.record_with(
            started,
            self.node.clone(),
            label.as_str(),
            answer.is_ok(),
            encoded,
        );
        answer
    }

    async fn claim_facts(
        &self,
        node: Option<&lash_durable::Owner>,
        actor: &ActorKey,
    ) -> Result<lash_durable::ClaimFacts, DurableError> {
        self.inner.claim_facts(node, actor).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }

    async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError> {
        self.inner.await_owner_commit(actor).await
    }

    async fn actors_in(
        &self,
        formats: &lash_durable::FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.actors_in(formats, after, limit).await
    }
}

#[async_trait::async_trait]
impl DurableReads for RecordingStore {
    async fn model_call_pins(
        &self,
        session: &lash_protocol_types::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
        self.inner.model_call_pins(session, owner).await
    }

    async fn model_calls(
        &self,
        session: &lash_protocol_types::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_core_store::model_calls::ModelCallRecord>, DurableError> {
        self.inner.model_calls(session, owner).await
    }

    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
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
    ) -> Result<Option<TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
        if matches!(owner, OwnerKey::Process(_)) {
            self.recorder.process_counters.facts();
        }
        self.inner.run_facts(owner).await
    }

    async fn run_facts_of(
        &self,
        owner: &OwnerKey,
        run: lash_durable::domain::RunSeq,
    ) -> Result<RunFacts, DurableError> {
        if matches!(owner, OwnerKey::Process(_)) {
            self.recorder.process_counters.facts();
        }
        self.inner.run_facts_of(owner, run).await
    }
    async fn run_facts_within(
        &self,
        owner: &lash_durable::domain::OwnerKey,
        run: Option<lash_durable::domain::RunSeq>,
        max_rows: usize,
        limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
    ) -> Result<lash_durable::domain::RunFacts, DurableError> {
        if matches!(owner, OwnerKey::Process(_)) {
            self.recorder.process_counters.facts();
        }
        self.inner
            .run_facts_within(owner, run, max_rows, limits)
            .await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        if matches!(exec, ExecKey::Process(_)) {
            self.recorder.process_counters.record("exec_root");
        }
        self.inner.exec_root(exec).await
    }

    async fn cell_roots(
        &self,
        session: &lash_protocol_types::SessionId,
        run: &lash_protocol_types::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        self.inner.cell_roots(session, run).await
    }
    async fn cell_roots_within(
        &self,
        session: &lash_protocol_types::SessionId,
        run: &lash_protocol_types::TurnId,
        max_rows: usize,
    ) -> Result<Vec<lash_durable::domain::ExecRootRow>, DurableError> {
        self.inner.cell_roots_within(session, run, max_rows).await
    }

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<lash_durable::domain::checkpoint_graph::NodeId>,
        limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        if matches!(&owner, RootOwner::Exec(ExecKey::Process(_))) {
            self.recorder.process_counters.record("hydrate");
        }
        self.inner.hydrate(owner, expected, limits).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
        self.recorder.process_counters.record("process");
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

    async fn prompt_snapshot(
        &self,
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}

impl RecordingStore {
    fn page_store(&self) -> Result<Arc<dyn PageStore>, DurableError> {
        self.inner.pages().ok_or_else(|| {
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Unavailable,
                message: "the store publishes no heap pages".to_owned(),
            })
        })
    }
}

/// Page publication: a candidate's commit is recorded as the owner
/// transaction it is; leases, reads and retirement pass through.
#[async_trait::async_trait]
impl PageStore for RecordingStore {
    async fn acquire_root_lease(
        &self,
        request: RootLeaseRequest,
    ) -> Result<RootLease, DurableError> {
        self.page_store()?.acquire_root_lease(request).await
    }

    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError> {
        self.page_store()?.renew_root_lease(lease).await
    }

    async fn release_root_lease(&self, lease: RootLeaseId) -> Result<(), DurableError> {
        self.page_store()?.release_root_lease(lease).await
    }

    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[lash_core_store::checkpoint_graph::NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        self.page_store()?.read_pages(lease, pages).await
    }

    async fn commit_candidate(
        &self,
        tx: ActorTx,
        candidate: CandidatePublication,
        label: CommitLabel,
    ) -> Result<CandidateReceipt, DurableError> {
        let pages = self.page_store()?;
        let actor = tx.actor().to_string();
        let mut encoded = Encoded::of(&tx);
        let started = Instant::now();
        let (answer, sql) = lash_core_ids::operational_metrics::sql::with_owner(
            actor.clone(),
            tx.epoch().0,
            tx.revision().0,
            lash_core_ids::operational_metrics::sql::collect(
                format!("{actor}/{}", label.as_str()),
                self.dialect,
                pages.commit_candidate(tx, candidate, label),
            ),
        )
        .await;
        encoded.sql = Some(sql);
        self.recorder
            .record_with(started, actor, label.as_str(), answer.is_ok(), encoded);
        answer
    }

    async fn candidate_state(
        &self,
        exec: &ExecKey,
    ) -> Result<
        Option<(
            lash_core_store::checkpoint_graph::NodeId,
            lash_core_store::checkpoint_graph::CommitId,
        )>,
        DurableError,
    > {
        self.page_store()?.candidate_state(exec).await
    }

    async fn retire_step(
        &self,
        budget: RetirementBudget,
    ) -> Result<RetirementProgress, DurableError> {
        let pages = self.page_store()?;
        self.retirement_batches.fetch_add(1, Ordering::Relaxed);
        pages.retire_step(budget).await
    }

    async fn retirement_backlog(&self) -> Result<RetirementBacklog, DurableError> {
        self.page_store()?.retirement_backlog().await
    }
}

/// A store set whose durable store is a [`RecordingStore`]; every other
/// port is the inner set's.
pub struct RecordingStores {
    inner: Arc<dyn StoreSet>,
    store: Arc<RecordingStore>,
}

impl RecordingStores {
    /// Retirement attempts through this store, including empty steps.
    pub fn retirement_batches(&self) -> u64 {
        self.store.retirement_batches.load(Ordering::Relaxed)
    }

    /// Record `inner`'s durable writes as `node`'s into `recorder`.
    pub fn new(
        inner: Arc<dyn StoreSet>,
        node: &str,
        recorder: Arc<Recorder>,
        dialect: &'static str,
    ) -> Self {
        let store = Arc::new(RecordingStore {
            retirement_batches: Arc::new(AtomicU64::new(0)),
            inner: inner.durable_store(),
            node: node.to_owned(),
            recorder,
            dialect,
        });
        Self { inner, store }
    }
}

impl StoreSet for RecordingStores {
    fn durable_store(&self) -> Arc<dyn DurableStore> {
        Arc::clone(&self.store) as _
    }

    fn wake_sink(&self) -> lash_durable::runner::WakeSink {
        self.inner.wake_sink()
    }

    fn node_wakes(&self) -> Option<Arc<dyn NodeWakes>> {
        self.inner.node_wakes()
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
        Arc::new(crate::process_registry::RecordingRegistry {
            inner: self.inner.process_registry(),
            counters: Arc::clone(&self.store.recorder.process_counters),
        })
    }

    fn process_authority_reads(
        &self,
    ) -> Arc<dyn lash_execution_contracts::backend::ProcessAuthorityReads> {
        self.inner.process_authority_reads()
    }

    fn process_env_store(&self) -> Arc<dyn ProcessEnvStore> {
        self.inner.process_env_store()
    }

    fn turn_prelude_store(
        &self,
    ) -> Arc<dyn lash_execution_contracts::turn_prelude::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }

    fn tool_material_store(&self) -> Arc<dyn lash_core_execution::store::ToolMaterialStore> {
        self.inner.tool_material_store()
    }

    fn definition_store(
        &self,
    ) -> Arc<dyn lash_execution_contracts::process::ProcessDefinitionStore> {
        self.inner.definition_store()
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.inner.module_artifacts()
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
