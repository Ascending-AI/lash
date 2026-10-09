//! The real-host E2E commit ledger: a store-set decorator a host composes
//! over its own store set, the way a host decorates any port.
//!
//! Every durable transaction the node runs goes through
//! [`DurableStore`](lash::durable::DurableStore) under a commit label. The
//! decorator forwards each one unchanged and appends what happened to a
//! case-owned JSON-lines ledger: the label, the actor, the epoch and whether
//! the store applied it. A case may also name cuts: the `nth` commit of a
//! label, held before or after the store applies it. A held commit's caller
//! never gets an answer until the controller releases the cut, so the
//! controller kills the node, or partitions it, at an exact committed phase.
//! A before-commit cut retains an issued request independently of its
//! caller: cancelling an actor cannot recall an in-flight store request.
//! Each line is synced before the next step is observable.
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use lash::durable::domain::*;
use lash::durable::*;
use lash::sync::MutexExt as _;
use serde::Deserialize;
use tokio::sync::watch;

/// One cut: the `nth` commit (from 1) of `label` whose actor key starts
/// with `actor`, held `before` or after the store applies it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    pub label: String,
    #[serde(default = "first")]
    pub nth: u32,
    #[serde(default)]
    pub before: bool,
    #[serde(default)]
    pub actor: Option<String>,
}

fn first() -> u32 {
    1
}

/// The ledger and its cuts, shared by the decorator and the host's control
/// route that releases a held cut.
pub struct CommitLedger {
    node: String,
    file: Mutex<std::fs::File>,
    cuts: Vec<Cut>,
    seen: Mutex<Vec<u32>>,
    released: watch::Sender<Vec<String>>,
}

impl CommitLedger {
    /// A ledger appending to `path` for `node`, holding at `cuts`.
    pub fn open(path: &Path, node: &str, cuts: Vec<Cut>) -> Result<Arc<Self>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open commit ledger {}", path.display()))?;
        Ok(Arc::new(Self {
            node: node.to_owned(),
            file: Mutex::new(file),
            seen: Mutex::new(vec![0; cuts.len()]),
            cuts,
            released: watch::Sender::new(Vec::new()),
        }))
    }

    /// The ledger the host's environment names: `ledger` is the file and
    /// `cuts` an optional JSON list of [`Cut`]s.
    pub fn from_env(ledger: &str, cuts: &str, node: &str) -> Result<Option<Arc<Self>>> {
        let Some(path) = std::env::var_os(ledger).map(PathBuf::from) else {
            return Ok(None);
        };
        let cuts = match std::env::var(cuts) {
            Ok(text) => serde_json::from_str(&text).context("decode commit cuts")?,
            Err(_) => Vec::new(),
        };
        Self::open(&path, node, cuts).map(Some)
    }

    /// Release the held cut of `label`; a cut released before it is reached
    /// passes straight through.
    pub fn release(&self, label: &str) {
        self.released
            .send_modify(|released| released.push(label.to_owned()));
    }

    fn append(&self, mut record: serde_json::Value) {
        // Two boots of one node name share its ledger; each line names its
        // boot.
        if let Some(fields) = record.as_object_mut() {
            fields.insert("boot".to_owned(), std::process::id().into());
        }
        let mut line = record.to_string().into_bytes();
        line.push(b'\n');
        let mut file = self.file.lock_recover();
        // A ledger write that fails loses evidence, never work: the case
        // reads a missing line as a failed oracle.
        let _ = file.write_all(&line).and_then(|()| file.sync_data());
    }

    fn record(
        &self,
        label: CommitLabel,
        actor: Option<&ActorKey>,
        epoch: Option<Epoch>,
        error: Option<&DurableError>,
    ) {
        self.append(serde_json::json!({
            "node": self.node,
            "label": label.as_str(),
            "actor": actor.map(ToString::to_string),
            "epoch": epoch.map(|epoch| epoch.0),
            "applied": error.is_none(),
            "error": error.map(|error| format!("{error:?}")),
        }));
    }

    /// One line per actor a reap released, naming the node it was taken from.
    fn reaped(&self, answer: &Result<Vec<Reaped>, DurableError>) {
        for reaped in answer.iter().flatten() {
            self.append(serde_json::json!({
                "node": self.node,
                "label": CommitLabel::REAP.as_str(),
                "actor": reaped.actor.to_string(),
                "epoch": reaped.epoch.0,
                "from": reaped.from.node.to_string(),
                "applied": true,
            }));
        }
    }

    /// The index of the cut this commit reaches, counting it.
    fn reached(&self, label: CommitLabel, actor: &ActorKey) -> Option<usize> {
        let mut seen = self.seen.lock_recover();
        let mut reached = None;
        for (index, cut) in self.cuts.iter().enumerate() {
            if cut.label == label.as_str()
                && cut
                    .actor
                    .as_ref()
                    .is_none_or(|prefix| actor.to_string().starts_with(prefix))
            {
                seen[index] += 1;
                if seen[index] == cut.nth && reached.is_none() {
                    reached = Some(index);
                }
            }
        }
        reached
    }

    /// Mark the cut held and wait for its release.
    async fn hold(&self, index: usize, actor: &ActorKey) {
        let cut = &self.cuts[index];
        self.append(serde_json::json!({
            "node": self.node,
            "held": cut.label,
            "before": cut.before,
            "nth": cut.nth,
            "actor": actor.to_string(),
        }));
        let mut released = self.released.subscribe();
        let _ = released
            .wait_for(|released| released.contains(&cut.label))
            .await;
        self.append(serde_json::json!({"node": self.node, "released": cut.label}));
    }
}

/// The host's store set with its durable store wrapped in [`LedgerStore`].
pub fn ledger_stores(
    inner: Arc<dyn lash::StoreSet>,
    ledger: Arc<CommitLedger>,
) -> Arc<dyn lash::StoreSet> {
    let durable: Arc<dyn DurableStore> = Arc::new(LedgerStore {
        inner: inner.durable_store(),
        ledger: Arc::clone(&ledger),
    });
    Arc::new(LedgerStores {
        inner,
        durable,
        ledger,
    })
}

struct LedgerStores {
    inner: Arc<dyn lash::StoreSet>,
    durable: Arc<dyn DurableStore>,
    ledger: Arc<CommitLedger>,
}

/// The store's node wakes, with the liveness reap a free lock allows logged
/// under the reap's label as a lease reap is.
struct LedgerNodeWakes {
    inner: Arc<dyn NodeWakes>,
    ledger: Arc<CommitLedger>,
}

#[lash::async_trait]
impl NodeWakes for LedgerNodeWakes {
    async fn publish(&self, batch: &WakeBatch) -> Result<(), DurableError> {
        self.inner.publish(batch).await
    }

    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        self.inner.listen(lease).await
    }

    async fn liveness(&self) -> Result<Vec<BootLiveness>, DurableError> {
        self.inner.liveness().await
    }

    async fn reap_released(
        &self,
        reaper: &NodeLease,
        boot: &Owner,
    ) -> Result<Vec<Reaped>, DurableError> {
        let answer = self.inner.reap_released(reaper, boot).await;
        self.ledger.reaped(&answer);
        answer
    }
}

impl lash::StoreSet for LedgerStores {
    fn binding_identity(&self) -> &lash::StoreBindingId {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash::runtime::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn lash::persistence::DeploymentStore> {
        self.inner.session_store_factory()
    }

    fn attachment_referrers(&self) -> Arc<dyn lash::persistence::AttachmentReferrers> {
        self.inner.attachment_referrers()
    }

    fn durable_store(&self) -> Arc<dyn DurableStore> {
        Arc::clone(&self.durable)
    }

    fn node_wakes(&self) -> Option<Arc<dyn NodeWakes>> {
        self.inner.node_wakes().map(|inner| {
            Arc::new(LedgerNodeWakes {
                inner,
                ledger: Arc::clone(&self.ledger),
            }) as Arc<dyn NodeWakes>
        })
    }

    fn process_registry(&self) -> Arc<dyn lash::persistence::ProcessRegistry> {
        self.inner.process_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash::persistence::ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn turn_prelude_store(&self) -> Arc<dyn lash::persistence::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }

    fn tool_material_store(&self) -> Arc<dyn lash::persistence::ToolMaterialStore> {
        self.inner.tool_material_store()
    }

    fn definition_store(&self) -> Arc<dyn lash::persistence::ProcessDefinitionStore> {
        self.inner.definition_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash::persistence::AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn lash::persistence::ModuleArtifactStore> {
        self.inner.module_artifacts()
    }

    fn recovery_leader(&self) -> Arc<dyn lash::persistence::RecoveryLeaderStore> {
        self.inner.recovery_leader()
    }

    fn obligation_ledger(
        &self,
        kind: lash::ObligationKind,
    ) -> Arc<dyn lash::persistence::ObligationLedger> {
        self.inner.obligation_ledger(kind)
    }

    fn artifact_cleanup(&self) -> Arc<dyn lash::persistence::ArtifactCleanupLedger> {
        self.inner.artifact_cleanup()
    }
}

/// The durable store that records every labelled transaction and holds at
/// the case's cuts. Every other call is the inner store's.
struct LedgerStore {
    inner: Arc<dyn DurableStore>,
    ledger: Arc<CommitLedger>,
}

#[lash::async_trait]
impl DurableReads for LedgerStore {
    async fn turn(&self, session: &lash::SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &lash::SessionId,
        run: &lash::TurnId,
    ) -> Result<Vec<TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &lash::SessionId,
        run: &lash::TurnId,
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

    async fn process(
        &self,
        process: &lash::ProcessId,
    ) -> Result<Option<ProcessActorRow>, DurableError> {
        self.inner.process(process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<lash::ProcessId>, DurableError> {
        self.inner.live_until_descendants(scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&lash::ProcessId>,
        limit: usize,
    ) -> Result<Vec<lash::ProcessId>, DurableError> {
        self.inner.until_children(scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &lash::SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.inner.session_close(session).await
    }

    async fn ending_scopes(
        &self,
        session: &lash::SessionId,
    ) -> Result<Vec<ScopeKey>, DurableError> {
        self.inner.ending_scopes(session).await
    }

    async fn session_mailbox(
        &self,
        session: &lash::SessionId,
    ) -> Result<SessionMailbox, DurableError> {
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
        call: &PromptCallKey,
    ) -> Result<Option<PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(&self, hashes: &[String]) -> Result<Vec<PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}

#[lash::async_trait]
impl DurableStore for LedgerStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        let answer = self.inner.register_node(spec).await;
        self.ledger.record(
            CommitLabel::NODE_REGISTER,
            None,
            None,
            answer.as_ref().err(),
        );
        answer
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.inner.heartbeat(node).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        let answer = self.inner.reap(reaper).await;
        self.ledger.reaped(&answer);
        answer
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        let answer = self.inner.release_node(node).await;
        self.ledger
            .record(CommitLabel::NODE_RELEASE, None, None, answer.as_ref().err());
        answer
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        let answer = self.inner.claim(node, limit).await;
        if let Ok(claimed) = &answer {
            for claimed in claimed {
                self.ledger.record(
                    CommitLabel::CLAIM,
                    Some(&claimed.actor),
                    Some(claimed.epoch),
                    None,
                );
            }
        }
        answer
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        let answer = self.inner.mark_draining(node).await;
        self.ledger
            .record(CommitLabel::NODE_DRAIN, None, None, answer.as_ref().err());
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
        let actor = tx.actor().clone();
        let epoch = tx.epoch();
        let cut = self.ledger.reached(label, &actor);
        if let Some(index) = cut.filter(|index| self.ledger.cuts[*index].before) {
            let ledger = Arc::clone(&self.ledger);
            let inner = Arc::clone(&self.inner);
            // The request has crossed the store port. Keep it alive when
            // lease loss drops the actor awaiting its answer, as a database
            // request already in flight can outlive its client future.
            return tokio::spawn(async move {
                ledger.hold(index, &actor).await;
                let answer = inner.commit(tx, label).await;
                ledger.record(label, Some(&actor), Some(epoch), answer.as_ref().err());
                answer
            })
            .await
            .unwrap_or(Err(DurableError::AckLost { label }));
        }
        let answer = self.inner.commit(tx, label).await;
        self.ledger
            .record(label, Some(&actor), Some(epoch), answer.as_ref().err());
        if let Some(index) = cut.filter(|index| !self.ledger.cuts[*index].before) {
            self.ledger.hold(index, &actor).await;
        }
        answer
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let answer = self.inner.commit_mail(tx, label).await;
        self.ledger.record(label, None, None, answer.as_ref().err());
        answer
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }
}
