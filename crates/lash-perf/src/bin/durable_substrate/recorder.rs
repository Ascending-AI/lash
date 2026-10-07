//! What the bench observes of the production runtime: a store-set
//! decorator whose durable store forwards every call unchanged and records
//! each write transaction's label, latency and payload, and the shared
//! recorder the scripted model and engine report to.
//!
//! The runner, the activations and the fences are the production ones; the
//! decorator only watches. Its own cost is one mutex push per transaction.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_core_execution::{
    AttachmentStore, Clock, DeploymentStore, ModuleArtifactStore, ProcessExecutionEnvStore,
    ProcessRegistry, StoreBindingId, StoreSet, TriggerStore,
};
use lash_durable::domain::{
    DomainWrite, ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow,
    ScopeKey, SessionCloseRow, SnapshotRow, SnapshotWrite, TurnEnd, TurnRow, TurnWrite, WaitId,
    WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableStore, Epoch, FormatSet, HeartbeatOutcome, MailCommit,
    MailTx, NodeLease, NodeSpec, Reaped, Signals,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ProcessId, SessionId, TurnId};
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
    /// Bytes of the turn checkpoints it wrote.
    pub checkpoint_bytes: usize,
    /// Bytes of the VM or engine snapshots it wrote.
    pub snapshot_bytes: usize,
    /// How many domain writes it carried.
    pub domain_writes: usize,
}

type Waiters = HashMap<(String, &'static str), oneshot::Sender<Instant>>;

/// What every node and every scripted component reports to.
pub struct Recorder {
    start: Instant,
    transactions: Mutex<Vec<Transaction>>,
    claims: Mutex<Vec<(Instant, String, String)>>,
    model_calls: Mutex<HashMap<String, Vec<Instant>>>,
    waiters: Mutex<Waiters>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            transactions: Mutex::default(),
            claims: Mutex::default(),
            model_calls: Mutex::default(),
            waiters: Mutex::default(),
        }
    }
}

impl Recorder {
    fn since(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.start).as_micros()).unwrap_or(u64::MAX)
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
        self.record_with(started, actor, label, ok, 0, 0, 0);
    }

    #[allow(clippy::too_many_arguments)]
    fn record_with(
        &self,
        started: Instant,
        actor: String,
        label: &'static str,
        ok: bool,
        checkpoint_bytes: usize,
        snapshot_bytes: usize,
        domain_writes: usize,
    ) {
        let ended = Instant::now();
        let transaction = Transaction {
            at_us: self.since(started),
            micros: u64::try_from(ended.saturating_duration_since(started).as_micros())
                .unwrap_or(u64::MAX),
            actor,
            label,
            ok,
            checkpoint_bytes,
            snapshot_bytes,
            domain_writes,
        };
        if ok {
            let waiter = self
                .waiters
                .lock_recover()
                .remove(&(transaction.actor.clone(), label));
            if let Some(waiter) = waiter {
                let _ = waiter.send(ended);
            }
        }
        self.transactions.lock_recover().push(transaction);
    }
}

fn payload_bytes(writes: &[DomainWrite]) -> (usize, usize) {
    writes
        .iter()
        .fold((0, 0), |(checkpoints, snapshots), write| match write {
            DomainWrite::Turn(TurnWrite::Advance { phase, .. }) => (
                checkpoints + phase.checkpoint().map_or(0, str::len),
                snapshots,
            ),
            DomainWrite::Snapshot(SnapshotWrite::Put { snapshot_ref, .. }) => {
                (checkpoints, snapshots + snapshot_ref.len())
            }
            _ => (checkpoints, snapshots),
        })
}

/// A durable store that records each write transaction.
pub struct RecordingStore {
    inner: Arc<dyn DurableStore>,
    node: String,
    recorder: Arc<Recorder>,
}

#[async_trait::async_trait]
impl DurableStore for RecordingStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
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

    async fn live_decodes(&self) -> Result<Vec<Vec<FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let actor = tx.actor().to_string();
        let (checkpoints, snapshots) = payload_bytes(tx.domain());
        let writes = tx.domain().len();
        let started = Instant::now();
        let answer = self.inner.commit(tx, label).await;
        self.recorder.record_with(
            started,
            actor,
            label.as_str(),
            answer.is_ok(),
            checkpoints,
            snapshots,
            writes,
        );
        answer
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let writes = tx.writes().len();
        let started = Instant::now();
        let answer = self.inner.commit_mail(tx, label).await;
        self.recorder.record_with(
            started,
            self.node.clone(),
            label.as_str(),
            answer.is_ok(),
            0,
            0,
            writes,
        );
        answer
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }
}

#[async_trait::async_trait]
impl DurableReads for RecordingStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        self.inner.run_records(owner).await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        self.inner.snapshot(exec).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
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

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.inner.park_events(after, limit).await
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

/// A store set whose durable store is a [`RecordingStore`]; every other
/// port is the inner set's.
pub struct RecordingStores {
    inner: Arc<dyn StoreSet>,
    store: Arc<RecordingStore>,
}

impl RecordingStores {
    /// Record `inner`'s durable writes as `node`'s into `recorder`.
    pub fn new(inner: Arc<dyn StoreSet>, node: &str, recorder: Arc<Recorder>) -> Self {
        let store = Arc::new(RecordingStore {
            inner: inner.durable_store(),
            node: node.to_owned(),
            recorder,
        });
        Self { inner, store }
    }
}

impl StoreSet for RecordingStores {
    fn durable_store(&self) -> Arc<dyn DurableStore> {
        Arc::clone(&self.store) as _
    }

    fn durable_signals(&self) -> Option<Arc<dyn Signals>> {
        self.inner.durable_signals()
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
        self.inner.process_registry()
    }

    fn trigger_store(&self) -> Arc<dyn TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn turn_prelude_store(&self) -> Arc<dyn lash_core_execution::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }

    fn tool_material_store(&self) -> Arc<dyn lash_core_execution::store::ToolMaterialStore> {
        self.inner.tool_material_store()
    }

    fn definition_store(&self) -> Arc<dyn lash_core_execution::ProcessDefinitionStore> {
        self.inner.definition_store()
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.inner.module_artifacts()
    }

    fn recovery_leader(&self) -> Arc<dyn lash_core_execution::store::RecoveryLeaderStore> {
        self.inner.recovery_leader()
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
