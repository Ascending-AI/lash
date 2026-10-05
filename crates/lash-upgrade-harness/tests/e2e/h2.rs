//! Controller wiring for H2. Public admission supplies input/run identities;
//! introspection supplies invocation identities and the original journal bytes.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use lash_upgrade_harness::e2e::{
    Step,
    case::{ArtifactIdentity, CaseLease, Channel, Leg, Permutation, StoreKind},
    cluster::{ClusterControl, LocalCluster},
    control::{
        Barrier, BarrierKind, BarrierProof, CleanupReceipt, Control, CoreControl, Fault,
        FaultReceipt, FileBarriers, ToolControl, WorkIdentity,
        callback::BodyCallbacks,
        transport::{CommandHold, StartHold, TransportCut, V7Proxy},
    },
    evidence::{Evidence, EvidenceReader},
    host::{HostAdapter, HostCommand, HostObservation, HostReady},
    host_adapters::workbench::WorkbenchHost,
};
use lash_upgrade_harness::node::tools::deliveries;
use lash_upgrade_harness::restate_view::RestateView;
use serde_json::json;
use tokio::sync::Mutex;

use super::tools::{Scenario, Snapshot};

#[derive(Clone, Copy)]
pub enum Row {
    Singleton,
    Partial,
    Batch,
    PreFinal,
    BeforeIntent,
    AfterIntent,
    Ranks,
    InlineLoser,
    DeferredLoser,
    RetirePending,
    CancelAtCapture,
    CancelAfterAdoption,
    PublicationCrash,
    RetainedRemoval,
}
impl Row {
    fn id(self) -> &'static str {
        match self {
            Self::Singleton => "S01",
            Self::Partial => "S02",
            Self::Batch => "S05",
            Self::PreFinal => "S08",
            Self::BeforeIntent | Self::AfterIntent => "S09",
            Self::Ranks => "S10",
            Self::InlineLoser | Self::DeferredLoser => "S11",
            Self::RetirePending => "S12",
            Self::CancelAtCapture | Self::CancelAfterAdoption => "S23",
            Self::PublicationCrash => "S31",
            Self::RetainedRemoval => "S32",
        }
    }
    fn slug(self) -> &'static str {
        match self {
            Self::Singleton => "s01",
            Self::Partial => "s02",
            Self::Batch => "s05",
            Self::PreFinal => "s08",
            Self::BeforeIntent => "s09-before-intent",
            Self::AfterIntent => "s09-after-intent",
            Self::Ranks => "s10",
            Self::InlineLoser => "s11-inline",
            Self::DeferredLoser => "s11-deferred",
            Self::RetirePending => "s12",
            Self::CancelAtCapture => "s23-capture-adoption",
            Self::CancelAfterAdoption => "s23-post-adoption",
            Self::PublicationCrash => "s31",
            Self::RetainedRemoval => "s32",
        }
    }
    fn labels(self) -> &'static [&'static str] {
        match self {
            Self::Singleton => &["echo"],
            Self::Partial => &["a", "b"],
            Self::Batch => &["a", "b", "c"],
            Self::PreFinal | Self::BeforeIntent | Self::AfterIntent => &["intent"],
            Self::Ranks => &["rank_one", "rank_two", "rank_three"],
            Self::InlineLoser | Self::DeferredLoser | Self::PublicationCrash => {
                &["winner", "loser"]
            }
            Self::RetirePending => &["gate"],
            Self::CancelAtCapture | Self::CancelAfterAdoption | Self::RetainedRemoval => {
                &["winner", "source"]
            }
        }
    }
    /// H3's transfer rows: each case's evidence is scoped to the logical
    /// Run's own segments.
    pub(super) fn transfers(self) -> bool {
        matches!(
            self,
            Self::RetirePending
                | Self::CancelAtCapture
                | Self::CancelAfterAdoption
                | Self::PublicationCrash
                | Self::RetainedRemoval
        )
    }
    fn receiver(self) -> bool {
        matches!(
            self,
            Self::PreFinal | Self::BeforeIntent | Self::AfterIntent | Self::Ranks
        )
    }
    // S10 holds rank1's declaration at the receiver's event append instead of
    // on the wire: a proxy hold would head-of-line block the shared
    // invocation stream, so no later Run record of rank2 could be journaled.
    fn receiver_holds_declarations(self) -> bool {
        matches!(self, Self::Ranks)
    }
}

pub(super) struct Shared {
    pub(super) host: Mutex<WorkbenchHost>,
    pub(super) successor: Mutex<Option<WorkbenchHost>>,
    pub(super) generation_drain: Mutex<Option<serde_json::Value>>,
    pub(super) view: RestateView,
    callbacks: Mutex<BodyCallbacks>,
    directory: PathBuf,
    delivery: PathBuf,
    deadline: Instant,
    row: Row,
    admitted: Mutex<Option<WorkIdentity>>,
    receiver: Mutex<Option<String>>,
    ready: Mutex<Option<HostReady>>,
    proxy: Mutex<V7Proxy>,
    admin: String,
    namespace: String,
    stores: Arc<dyn lash::StoreSet>,
    pub(super) postgres: Option<lash_postgres_store::PostgresStorage>,
    pub(super) store_root: PathBuf,
    pub(super) chat: Mutex<Option<String>>,
    artifacts: Vec<ArtifactIdentity>,
    /// The successor's advertised transport, when the row owns one.
    successor_proxy: Mutex<Option<V7Proxy>>,
    successor_uri: Option<String>,
    /// Each bound logical Run's turn-invocation key, without its segment
    /// ordinal: every physical segment of that Run, and nothing else, has it.
    run_keys: Mutex<BTreeMap<String, String>>,
    /// Fenced follow-ons as first read. The session head moves on once the
    /// successor commits, so a transfer row keeps what publication showed.
    retained: Mutex<Vec<lash_upgrade_harness::e2e::evidence::RetainedTransferFact>>,
    /// Set once the predecessor is retired or killed for good: every later
    /// follow and resolution goes to the successor, the only host left.
    pub(super) predecessor_gone: Mutex<bool>,
    /// A row with a successor boots N+1 when it requests the drain, not before.
    successor_boot: Mutex<Option<(ArtifactIdentity, CaseLease)>>,
}
impl Shared {
    async fn journals(&self, work: &WorkIdentity) -> Result<Evidence> {
        #[derive(serde::Deserialize)]
        struct Invocation {
            id: String,
            pinned_service_protocol_version: Option<u32>,
        }
        let rows: Vec<Invocation> = if self.row.transfers() {
            // Only this logical Run's segments; one Restate has not started
            // yet has no journal.
            self.segments(&work.run)
                .await?
                .into_iter()
                .filter(|segment| segment.pinned_service_protocol_version.is_some())
                .map(|segment| Invocation {
                    id: segment.id,
                    pinned_service_protocol_version: segment.pinned_service_protocol_version,
                })
                .collect()
        } else {
            // A case owns one chat and no other turn. Include every actual segment
            // after handover, retaining each invocation's own journal provenance.
            let prefix = self.view.service_name("LashTurn").replace('\'', "''");
            self.view.query(&format!(
                "SELECT id, pinned_service_protocol_version FROM sys_invocation WHERE target_service_name LIKE '{prefix}%' AND target_handler_name = 'run' ORDER BY created_at"
            )).await?
        };
        ensure!(
            !rows.is_empty(),
            "actual admitted turn invocation is absent"
        );
        let mut evidence = Evidence::empty(self.row.slug().into());
        evidence.artifacts = self.artifacts.clone();
        for row in rows {
            ensure!(
                row.pinned_service_protocol_version == Some(7),
                "actual segment is not V7"
            );
            // A transfer row's barriers name the admitted work; each fact
            // still retains the actual invocation it was read from.
            let mut segment = work.clone();
            if !self.row.transfers() {
                segment.segment = row.id.clone();
            }
            evidence
                .journals
                .extend(self.view.journal(&segment, &row.id, 7).await?);
        }
        for delivery in deliveries(&self.delivery)? {
            evidence
                .effects
                .push(json!({"kind":"h2_body_delivery", "delivery":delivery}));
        }
        self.retained_transfers(work, &mut evidence).await?;
        self.retained_cancel(work, &mut evidence).await?;
        let host = self.host.lock().await;
        evidence.effects.extend(host.trace_records()?);
        if let Some(receipt) = self.generation_drain.lock().await.as_ref() {
            evidence.effects.push(receipt.clone());
        }
        if let Some(process) = self.receiver.lock().await.as_ref() {
            evidence.effects.push(
                host.control(
                    reqwest::Method::GET,
                    &format!("/api/e2e/receiver/{process}/receipts"),
                    None,
                )
                .await?,
            );
        }
        Ok(evidence)
    }
    /// The store evidence names: the SQLite database file, or the case
    /// database's PostgreSQL catalog identity.
    fn store_identity(&self) -> serde_json::Value {
        match &self.postgres {
            Some(storage) => json!(format!("postgres:{}", storage.catalog_id())),
            None => json!(self.store_root.join("durable-core.db")),
        }
    }
    async fn retained_cancel(&self, work: &WorkIdentity, evidence: &mut Evidence) -> Result<()> {
        let chat = self
            .chat
            .lock()
            .await
            .clone()
            .context("no bound native session")?;
        let session = lash::SessionId::parse(chat)?;
        let run = lash::TurnId::parse(&work.run)?;
        let store = self.stores.session_store_factory();
        // The cancel is recorded on the physical turn of the Run that was
        // running when it landed: the first one with a request, before the
        // first that has not committed.
        let mut ordinal = 0;
        let found = loop {
            let address = lash::TurnAddress::new(
                session.clone(),
                lash_core::store::PhysicalTurn::derive_turn_id(&run, ordinal),
            );
            if let Some(record) = store.turn_cancel_request(&address).await? {
                break Some(record);
            }
            if !store.turn_is_committed(&address).await? {
                break None;
            }
            ordinal += 1;
        };
        if let Some(record) = found {
            let artifact = self.directory.join("native-cancel-request.json");
            super::write(&artifact, &record)?;
            evidence
                .stores
                .push(json!({"kind":"h2_native_cancel_request",
                "artifact":artifact,"store":self.store_identity(),"record":record}));
        }
        Ok(())
    }
    async fn retained_transfers(&self, work: &WorkIdentity, evidence: &mut Evidence) -> Result<()> {
        let chat = self
            .chat
            .lock()
            .await
            .clone()
            .context("no bound native session")?;
        let session = lash::SessionId::parse(&chat)?;
        let rows: Vec<(String, String)> = match &self.postgres {
            Some(storage) => {
                sqlx::query_as::<_, (String, String)>(
                    "SELECT turn_id,result_json FROM lash_runtime_turn_commits WHERE session_id=$1 ORDER BY change_seq",
                )
                .bind(chat)
                .fetch_all(storage.pool())
                .await?
            }
            None => {
                let path = self.store_root.join("durable-core.db");
                tokio::task::spawn_blocking(move || -> Result<Vec<(String,String)>> {
                    let db = rusqlite::Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    let mut query = db.prepare("SELECT turn_id,result_json FROM runtime_turn_commits WHERE session_id=?1 ORDER BY change_seq")?;
                    Ok(query.query_map([chat], |row| Ok((row.get(0)?,row.get(1)?)))?.collect::<std::result::Result<_,_>>()?)
                }).await??
            }
        };
        if self.row.transfers() {
            // Which logical turns committed, for a missed-publication diagnosis.
            let turns: Vec<_> = rows.iter().map(|(turn, _)| turn.clone()).collect();
            super::write(
                &self.directory.join("native-commit-turns.json"),
                &json!({"run":work.run,"turns":turns}),
            )?;
        }
        for (turn, raw) in rows {
            // A commit's key names the logical turn it belongs to in its scope.
            // A follow-on's own commits re-carry the continuation it still
            // owes; only the Run's own commit publishes it.
            if serde_json::from_str::<serde_json::Value>(&turn)
                .ok()
                .and_then(|key| key.pointer("/scope/turn_id").cloned())
                != Some(json!(work.run))
            {
                continue;
            }
            let receipt = lash_core::store::decode_runtime_commit_receipt(&session, &turn, &raw)?;
            if let Some(follow_on) = &receipt.pending_follow_on
                && follow_on
                    .owes
                    .continuation()
                    .and_then(|c| c.opener.run.as_deref())
                    .is_some()
            {
                let artifact = self.directory.join(format!(
                    "native-transfer-{}.json",
                    lash_core::stable_hash::sha256_hex(turn.as_bytes())
                ));
                super::write(
                    &artifact,
                    &json!({"session_id":session,"turn_id":turn,"receipt":receipt}),
                )?;
                evidence.retain_follow_on(
                    work.clone(),
                    follow_on,
                    artifact.display().to_string(),
                )?;
            }
        }
        if self.row.transfers() {
            let mut retained = self.retained.lock().await;
            for fact in std::mem::take(&mut evidence.transfers) {
                if !retained
                    .iter()
                    .any(|seen| seen.artifact == fact.artifact && seen.transfer == fact.transfer)
                {
                    retained.push(fact);
                }
            }
            evidence.transfers = retained
                .iter()
                .filter(|fact| fact.work.run == work.run)
                .cloned()
                .collect();
        }
        Ok(())
    }
    async fn bind(&self, work: WorkIdentity, chat: &str) -> Result<WorkIdentity> {
        let session = lash::SessionId::parse(chat)?;
        let run = lash::TurnId::parse(&work.run)?;
        let store = self.stores.session_store_factory();
        while store.run_executor(&session, &run).await?.is_none() {
            ensure!(
                Instant::now() < self.deadline,
                "accepted input never acquired its native Run executor"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut reader = lash_upgrade_harness::e2e::evidence::RestateEvidenceReader::new(
            self.row.slug().into(),
            RestateView::new(&self.admin, &self.namespace)?,
            7,
        );
        let key = lash_restate::recorded_turn_invocation_key(store.as_ref(), &session, &run)
            .await?
            .context("accepted Run has no retained turn invocation key")?;
        let key = key
            .rsplit_once('#')
            .map_or(key.clone(), |(prefix, _)| prefix.to_owned());
        self.run_keys.lock().await.insert(work.run.clone(), key);
        let work = reader
            .bind_public_run(store.as_ref(), &session, &run, work.ingress)
            .await?;
        self.proxy
            .lock()
            .await
            .bind_invocation(work.segment.clone(), work.clone())?;
        let callbacks = self.callbacks.lock().await;
        callbacks.bind(work.run.clone(), work.clone())?;
        for label in self.row.labels() {
            callbacks
                .await_delivery(&work.run, label, 1, BarrierKind::BodyEntered)
                .await?;
        }
        *self.chat.lock().await = Some(chat.into());
        *self.admitted.lock().await = Some(work.clone());
        Ok(work)
    }
}

/// Whether a physical invocation key is a segment of the logical Run `run`
/// admitted under `key`: an admission of its input, or a transferred segment.
fn segment_of(candidate: &str, key: &str, run: &str) -> bool {
    candidate == key
        || candidate
            .strip_prefix(key)
            .is_some_and(|ordinal| ordinal.starts_with('#'))
        || candidate.contains(&transferred(run))
}

/// The admission fragment of every segment a successor runs `run` under.
pub(super) fn transferred(run: &str) -> String {
    format!("run:{run}#")
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct Segment {
    pub(super) id: String,
    pub(super) target_service_name: String,
    pub(super) target_service_key: String,
    pub(super) status: String,
    pub(super) pinned_deployment_id: Option<String>,
    pub(super) pinned_service_protocol_version: Option<u32>,
}

impl Shared {
    /// Every physical `run` invocation of the logical Run, oldest first.
    pub(super) async fn segments(&self, run: &str) -> Result<Vec<Segment>> {
        let key = self.key_of(run).await?;
        let prefix = self.view.service_name("LashTurn").replace('\'', "''");
        let rows: Vec<Segment> = self.view.query(&format!(
            "SELECT id, target_service_name, target_service_key, status, pinned_deployment_id, pinned_service_protocol_version FROM sys_invocation WHERE target_service_name LIKE '{prefix}%' AND target_handler_name = 'run' ORDER BY created_at"
        )).await?;
        super::write(&self.directory.join("turn-invocations.json"), &rows)?;
        Ok(rows
            .into_iter()
            .filter(|row| segment_of(&row.target_service_key, &key, run))
            .collect())
    }
    async fn key_of(&self, run: &str) -> Result<String> {
        self.run_keys
            .lock()
            .await
            .get(run)
            .cloned()
            .context("logical Run has no bound invocation key")
    }
    pub(super) async fn successor_deployment(
        &self,
    ) -> Result<lash_upgrade_harness::restate_view::Deployment> {
        self.view
            .deployment_at(
                self.successor_uri
                    .as_deref()
                    .context("row has no advertised successor transport")?,
            )
            .await
    }
    /// The successor segment, once it adopted the transfer and parked on its
    /// own wait: pinned to the successor deployment and suspended there.
    async fn successor_admitted(&self, work: &WorkIdentity) -> Result<PathBuf> {
        let deployment = self.successor_deployment().await?;
        loop {
            let segments = self.segments(&work.run).await?;
            if let Some(segment) = segments.iter().find(|segment| {
                segment.target_service_key.contains(&transferred(&work.run))
                    && segment.pinned_deployment_id.as_deref() == Some(deployment.id.as_str())
                    && segment.status == "suspended"
            }) {
                let artifact = self
                    .directory
                    .join(format!("successor-admitted-{}.json", segment.id));
                super::write(
                    &artifact,
                    &json!({"run":work.run,"predecessor":work.segment,"successor":segment,
                        "deployment":{"id":deployment.id,"endpoint":deployment.endpoint},"segments":segments}),
                )?;
                return Ok(artifact);
            }
            ensure!(
                Instant::now() < self.deadline,
                "successor never adopted the transfer: {segments:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    /// The operator floor: the successor's production drain report must show
    /// the predecessor generation drained before its only deployment goes.
    async fn drain_and_retire(&self, generation: &str) -> Result<serde_json::Value> {
        let status = loop {
            let value = self
                .successor
                .lock()
                .await
                .as_ref()
                .context("no replacing workbench generation")?
                .control(
                    reqwest::Method::GET,
                    &format!("/api/e2e/generations/{generation}/drain"),
                    None,
                )
                .await?;
            let status: lash::GenerationDrainStatus = serde_json::from_value(value.clone())?;
            if status.drained() {
                break value;
            }
            ensure!(
                Instant::now() < self.deadline,
                "the predecessor generation never drained: {value}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let deployments = self.view.deployments_of_generation(generation).await?;
        let predecessor = self.proxy.lock().await.endpoint.clone();
        ensure!(
            deployments.len() == 1
                && deployments[0].endpoint.trim_end_matches('/')
                    == predecessor.trim_end_matches('/'),
            "the predecessor generation is not exactly the predecessor deployment: {deployments:?}"
        );
        let deployment = deployments[0].clone();
        self.view.remove_deployment(&deployment.id).await?;
        ensure!(
            self.view
                .deployments_of_generation(generation)
                .await?
                .is_empty(),
            "retired deployment is still registered"
        );
        let cleanup = self.host.lock().await.stop().await?;
        ensure!(
            !cleanup.is_empty() && cleanup.iter().all(|receipt| receipt.closed),
            "the retired workbench did not shut down cleanly"
        );
        *self.predecessor_gone.lock().await = true;
        Ok(
            json!({"kind":"h3_generation_retired","generation":generation,"drain":status,
            "deployment":{"id":deployment.id,"endpoint":deployment.endpoint},"cleanup":cleanup}),
        )
    }
    /// The authoritative store's view of one logical Run: its terminal, the
    /// follow-on the fenced session head still owes it, and whether it is
    /// the session's unfinished Run.
    pub(super) async fn run_snapshot(&self, run: &str) -> Result<serde_json::Value> {
        let chat = self
            .chat
            .lock()
            .await
            .clone()
            .context("no bound native session")?;
        let session = lash::SessionId::parse(&chat)?;
        let turn = lash::TurnId::parse(run)?;
        let store = self.stores.session_store_factory();
        let terminal = store.run_terminal(&session, &turn).await?;
        let unfinished = store
            .unfinished_run(&session)
            .await?
            .is_some_and(|unfinished| unfinished.run == turn);
        // The follow-on the fenced session head still owes, if it is this Run's.
        let owed: Option<String> = match &self.postgres {
            Some(storage) => sqlx::query_scalar::<_, Option<String>>(
                "SELECT pending_follow_on_json FROM lash_session_head WHERE session_id=$1",
            )
            .bind(&chat)
            .fetch_optional(storage.pool())
            .await?
            .flatten(),
            None => {
                let path = self.store_root.join("durable-core.db");
                tokio::task::spawn_blocking(move || -> Result<Option<String>> {
                    let db = rusqlite::Connection::open_with_flags(
                        path,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )?;
                    let mut query = db.prepare(
                        "SELECT pending_follow_on_json FROM session_head WHERE session_id=?1",
                    )?;
                    Ok(query
                        .query_map([chat], |row| row.get::<_, Option<String>>(0))?
                        .next()
                        .transpose()?
                        .flatten())
                })
                .await??
            }
        };
        let continuation = match owed {
            Some(raw) if raw.contains(run) => serde_json::from_str(&raw)?,
            _ => serde_json::Value::Null,
        };
        let snapshot = json!({"run":run,"terminal":terminal,"continuation":continuation,"unfinished":unfinished});
        super::write(
            &self.directory.join(format!(
                "run-snapshot-{}.json",
                lash_core::stable_hash::sha256_hex(run.as_bytes())
            )),
            &snapshot,
        )?;
        Ok(snapshot)
    }
}

struct Host(Arc<Shared>);
impl HostAdapter for Host {
    fn boot<'a>(
        &'a mut self,
        artifact: &'a ArtifactIdentity,
        lease: &'a mut CaseLease,
    ) -> Step<'a, HostReady> {
        Box::pin(async move {
            let mut host = self.0.host.lock().await;
            let previous = self.0.ready.lock().await.clone();
            let ready = if previous.is_some() {
                serde_json::from_value(
                    host.command(HostCommand::Process {
                        action: "restart".into(),
                        input: json!({}),
                    })
                    .await?
                    .output,
                )?
            } else {
                host.boot(artifact, lease).await?
            };
            if previous.is_none() && self.0.row.receiver() {
                let chat = host.command(HostCommand::Process { action:"create-session".into(),
                    input:json!({"session":format!("{}-{}",lease.namespace,self.0.row.id().to_lowercase())}) }).await?;
                let receipt = host
                    .command(HostCommand::Process {
                        action: "register-receiver".into(),
                        input: chat.output,
                    })
                    .await?;
                *self.0.receiver.lock().await = Some(
                    receipt.output["process_id"]
                        .as_str()
                        .context("receiver did not register an actual process")?
                        .into(),
                );
            }
            if previous.is_none() && self.0.successor.lock().await.is_some() {
                // N+1 registers only when the roll begins. Registered first,
                // it would take the session's admission and run the turn itself.
                *self.0.successor_boot.lock().await = Some((
                    artifact.clone(),
                    CaseLease {
                        gate_id: lease.gate_id.clone(),
                        namespace: lease.namespace.clone(),
                        authority: lease.authority.clone(),
                        directory: lease.directory.join("successor"),
                        postgres_url: lease.postgres_url.clone(),
                        ports: lease.ports.clone(),
                        deadline: lease.deadline,
                        processes: Vec::new(),
                        cleanup: Vec::new(),
                    },
                ));
            }
            *self.0.ready.lock().await = Some(ready.clone());
            Ok(ready)
        })
    }
    fn command<'a>(&'a mut self, command: HostCommand) -> Step<'a, HostObservation> {
        Box::pin(async move {
            if let HostCommand::Transfer { run } = &command {
                let admitted = self
                    .0
                    .admitted
                    .lock()
                    .await
                    .clone()
                    .context("no bound admission")?;
                ensure!(
                    admitted.run == *run,
                    "handover targeted another logical Run"
                );
                if let Some((artifact, mut next_lease)) = self.0.successor_boot.lock().await.take()
                {
                    std::fs::create_dir_all(&next_lease.directory)?;
                    let next = self
                        .0
                        .successor
                        .lock()
                        .await
                        .as_mut()
                        .context("no replacing workbench generation")?
                        .boot(&artifact, &mut next_lease)
                        .await?;
                    let ready = self
                        .0
                        .ready
                        .lock()
                        .await
                        .clone()
                        .context("no live owned host")?;
                    ensure!(
                        next.process.pid != ready.process.pid,
                        "successor reused the predecessor process"
                    );
                }
                // Draining N cuts only a Run N executes. Had N+1 registered
                // before the input, it would have admitted and run the turn.
                let endpoint = self.0.proxy.lock().await.endpoint.clone();
                let deployment = self.0.view.deployment_at(&endpoint).await?;
                let segments = self.0.segments(run).await?;
                ensure!(
                    segments.iter().any(|segment| segment.id == admitted.segment
                        && segment.pinned_deployment_id.as_deref() == Some(deployment.id.as_str())),
                    "the drained generation does not execute the Run: {segments:?}"
                );
                let predecessor = self.0.host.lock().await;
                let generation = predecessor
                    .control(reqwest::Method::GET, "/api/e2e/generation", None)
                    .await?;
                let generation: lash::BuildGeneration = serde_json::from_value(generation)?;
                let mut next = self.0.successor.lock().await;
                let next = next.as_mut().context("no replacing workbench generation")?;
                let next_generation = next
                    .control(reqwest::Method::GET, "/api/e2e/generation", None)
                    .await?;
                let next_generation: lash::BuildGeneration =
                    serde_json::from_value(next_generation)?;
                ensure!(
                    generation != next_generation,
                    "successor shares the predecessor generation"
                );
                let output = next
                    .control(
                        reqwest::Method::POST,
                        &format!("/api/e2e/generations/{generation}/drain"),
                        None,
                    )
                    .await?;
                ensure!(
                    output["changed"] == true,
                    "generation was not newly marked draining"
                );
                let receipt = json!({"kind":"h2_generation_drain","generation":generation,"successor_generation":next_generation,"output":output});
                *self.0.generation_drain.lock().await = Some(receipt);
                return Ok(HostObservation {
                    work: admitted,
                    output,
                });
            }
            if let HostCommand::Attach { run } = &command
                && *self.0.predecessor_gone.lock().await
            {
                // Only the successor deployment remains; follow through it.
                let admitted = self
                    .0
                    .admitted
                    .lock()
                    .await
                    .clone()
                    .context("no bound admission")?;
                ensure!(admitted.run == *run, "follow targeted another logical Run");
                let chat = self
                    .0
                    .chat
                    .lock()
                    .await
                    .clone()
                    .context("no bound session")?;
                let next = self.0.successor.lock().await;
                let next = next.as_ref().context("no replacing workbench generation")?;
                let outcome = next
                    .control(
                        reqwest::Method::GET,
                        &format!("/api/e2e/sessions/{chat}/inputs/{}", admitted.ingress),
                        None,
                    )
                    .await?;
                return Ok(HostObservation {
                    work: admitted,
                    output: json!({"outcome":outcome}),
                });
            }
            let submitting = matches!(command, HostCommand::Submit { .. });
            let mut observation = self.0.host.lock().await.command(command).await?;
            if submitting {
                observation.work = self
                    .0
                    .bind(
                        observation.work,
                        observation.output["session_id"]
                            .as_str()
                            .context("Submit has no actual session id")?,
                    )
                    .await?;
            } else if !observation.work.run.is_empty() {
                let admitted = self.0.admitted.lock().await;
                let admitted = admitted.as_ref().context("host has no bound admission")?;
                ensure!(
                    observation.work.run == admitted.run,
                    "host observation changed actual logical run"
                );
                observation.work.segment = admitted.segment.clone();
            }
            Ok(observation)
        })
    }
    fn transcript(&self) -> Result<Vec<HostObservation>> {
        bail!("use the owned async host transcript in evidence")
    }
    fn stop(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            // Teardown both owned processes even if one stop reports failure.
            let first = self.0.host.lock().await.stop().await;
            let next = if let Some(host) = self.0.successor.lock().await.as_mut() {
                host.stop().await
            } else {
                Ok(Vec::new())
            };
            let mut cleanup = first?;
            cleanup.extend(next?);
            Ok(cleanup)
        })
    }
}
struct Reader(Snapshot);
impl EvidenceReader for Reader {
    fn collect<'a>(&'a mut self, work: &'a WorkIdentity) -> Step<'a, Evidence> {
        (self.0)(work.clone())
    }
}
struct Controller {
    core: CoreControl,
    shared: Arc<Shared>,
    observed: Vec<BarrierProof>,
}
impl Control for Controller {
    fn await_barrier<'a>(&'a mut self, barrier: &'a Barrier) -> Step<'a, BarrierProof> {
        Box::pin(async move {
            if barrier.kind == BarrierKind::ContinuationPublished {
                loop {
                    let evidence = self.shared.journals(&barrier.work).await?;
                    if let Some(fact) = evidence.transfers.first() {
                        self.core
                            .barriers
                            .publish_store(barrier, &PathBuf::from(&fact.artifact))?;
                        break;
                    }
                    if self.shared.row.transfers() {
                        let snapshot = self.shared.run_snapshot(&barrier.work.run).await?;
                        ensure!(
                            snapshot["terminal"].is_null(),
                            "the Run ended without publishing its continuation: {snapshot}"
                        );
                    }
                    ensure!(
                        Instant::now() < self.shared.deadline,
                        "continuation publication was absent"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            if barrier.kind == BarrierKind::SuccessorAdmitted {
                let artifact = self.shared.successor_admitted(&barrier.work).await?;
                self.core.barriers.publish_store(barrier, &artifact)?;
            }
            let proof = self.core.await_barrier(barrier).await?;
            self.observed.push(proof.clone());
            Ok(proof)
        })
    }
    fn inject<'a>(&'a mut self, fault: Fault, proof: &'a BarrierProof) -> Step<'a, FaultReceipt> {
        Box::pin(async move {
            ensure!(
                self.observed.iter().any(|p| p.barrier == proof.barrier
                    && p.artifact == proof.artifact
                    && p.journal_index == proof.journal_index),
                "fault lacks an observed exact barrier"
            );
            if let Fault::DrainAndRetire { generation } = &fault {
                let incarnation = self
                    .shared
                    .ready
                    .lock()
                    .await
                    .as_ref()
                    .context("no live owned host")?
                    .process
                    .incarnation;
                let receipt = self.shared.drain_and_retire(generation).await?;
                let artifact = self.shared.directory.join("generation-retired.json");
                super::write(&artifact, &receipt)?;
                return Ok(FaultReceipt {
                    fault,
                    proof: proof.clone(),
                    target_incarnation: incarnation,
                });
            }
            let Fault::KillHost { target } = &fault else {
                bail!("H2 only kills its owned workbench host")
            };
            let ready = self
                .shared
                .ready
                .lock()
                .await
                .clone()
                .context("no live owned host")?;
            ensure!(target == &ready.process.role, "kill targeted another host");
            let killed = self
                .shared
                .host
                .lock()
                .await
                .command(HostCommand::Process {
                    action: "kill-host".into(),
                    input: json!({}),
                })
                .await?;
            ensure!(
                killed.output["killed"] == true
                    && killed.output["reaped"] == true
                    && killed.output["process"]["pid"] == ready.process.pid,
                "kill did not reap the actual selected child"
            );
            ensure!(
                killed.output["cleanup"]
                    .as_array()
                    .context("kill cleanup missing")?
                    .iter()
                    .all(|r| r["closed"] == true),
                "kill leaked an owned resource"
            );
            Ok(FaultReceipt {
                fault,
                proof: proof.clone(),
                target_incarnation: ready.process.incarnation,
            })
        })
    }
    fn tool<'a>(&'a mut self, command: ToolControl) -> Step<'a, ()> {
        Box::pin(async move {
            if let ToolControl::Resolve { work, value } = command {
                let matches: Vec<_> = deliveries(&self.shared.delivery)?
                    .into_iter()
                    .filter(|d| work.call.as_deref() == Some(d.call_id.as_str()))
                    .collect();
                ensure!(
                    matches.len() == 1,
                    "resolution needs exactly one actual Deferred descriptor"
                );
                let key = matches[0]
                    .completion
                    .as_ref()
                    .context("body did not reserve a completion descriptor")?;
                let resolve = HostCommand::Process {
                    action: "resolve".into(),
                    input: json!({"key":key,"value":value}),
                };
                if *self.shared.predecessor_gone.lock().await {
                    self.shared
                        .successor
                        .lock()
                        .await
                        .as_mut()
                        .context("no replacing workbench generation")?
                        .command(resolve)
                        .await?;
                } else {
                    self.shared.host.lock().await.command(resolve).await?;
                }
                Ok(())
            } else {
                if let ToolControl::Hold(barrier) = &command
                    && barrier.kind == BarrierKind::PredecessorDischarged
                {
                    // The predecessor's successor start is its last command
                    // before Output: holding it keeps the discharge owed.
                    self.shared
                        .proxy
                        .lock()
                        .await
                        .arm_command_hold(CommandHold {
                            invocation: barrier.work.segment.clone(),
                            command: lash_restate_test::protocol::MessageType::OneWayCallCommand,
                            barrier: barrier.clone(),
                        })?;
                }
                if let ToolControl::Hold(barrier) = &command
                    && barrier.kind == BarrierKind::SuccessorStarting
                {
                    let key_fragment = transferred(&barrier.work.run);
                    self.shared
                        .successor_proxy
                        .lock()
                        .await
                        .as_ref()
                        .context("row has no successor transport")?
                        .arm_start_hold(StartHold {
                            key_fragment,
                            barrier: barrier.clone(),
                        })?;
                }
                if let ToolControl::Hold(barrier) = &command
                    && (barrier.kind == BarrierKind::VProposed
                        || (barrier.kind == BarrierKind::DeclarationIssued
                            && !self.shared.row.receiver_holds_declarations()))
                {
                    self.shared.proxy.lock().await.arm_cut(TransportCut {
                        proposal: barrier.clone(),
                        before_ack: Barrier {
                            work: barrier.work.clone(),
                            kind: BarrierKind::BeforeAck,
                        },
                    })?;
                }
                self.core.tool(command).await
            }
        })
    }
}

pub async fn run(row: Row, permutation: Permutation) -> Result<()> {
    let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
    std::fs::create_dir_all(&root)?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let slug = format!(
        "{}{}{}",
        row.slug(),
        if permutation.store == StoreKind::PostgreSql {
            "-postgresql"
        } else {
            ""
        },
        if permutation.leg == Leg::Replay {
            "-replay"
        } else {
            ""
        }
    );
    let mut lease = CaseLease::new(&slug, root.join(&slug), deadline)?;
    let base: u16 = std::env::var("LASH_E2E_PORT_BASE")?.parse()?;
    ensure!(base <= u16::MAX - 50, "private port range overflow");
    lease.ports = (base + 10..base + 14).collect();
    let needs_successor = matches!(row, Row::InlineLoser | Row::DeferredLoser) || row.transfers();
    if needs_successor {
        lease.ports.extend([base + 20, base + 21]);
    }
    if needs_successor {
        lease.ports.push(base + 22);
    }
    let server = super::artifact(
        "restate-server",
        std::env::var("LASH_RESTATE_SERVER_BIN")?.into(),
    )?;
    let artifact = super::artifact(
        "agent-workbench",
        std::env::var("LASH_WORKBENCH_E2E_BIN")?.into(),
    )?;
    let postgres_url = permutation.postgres_url(&mut lease).await?;
    let mut cluster = LocalCluster::new(base, deadline).with_leg(permutation.leg);
    let boot = cluster.boot(&server, 1, &mut lease).await?;
    let callback_dir = lease.directory.join("barriers");
    std::fs::create_dir_all(&callback_dir)?;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, base + 12))?;
    let callbacks = BodyCallbacks::start(listener, callback_dir.clone(), deadline).await?;
    let proxy = V7Proxy::start(
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, base + 13))?,
        std::net::SocketAddr::from(([127, 0, 0, 1], base + 11)),
        callback_dir.clone(),
        deadline,
        Vec::new(),
    )
    .await?;
    let advertised_uri = proxy.endpoint.clone();
    let delivery = lease.directory.join("tool-deliveries.jsonl");
    let fixture = json!({"scenario":row.id(), "delivery_ledger":delivery,
        "provider_ledger":lease.directory.join("provider.jsonl"), "body_callback_url":callbacks.endpoint,
        "deferred_loser":matches!(row,Row::DeferredLoser | Row::PublicationCrash),
        "receiver_hold":row.receiver_holds_declarations()});
    let protocol =
        if matches!(row, Row::Ranks | Row::InlineLoser | Row::DeferredLoser) || row.transfers() {
            "rlm"
        } else {
            "standard"
        };
    let fixture_path = lease.directory.join("tool-fixture.json");
    super::write(&fixture_path, &fixture)?;
    let mut environment = BTreeMap::from([
        (
            "AGENT_WORKBENCH_TOOL_FIXTURE".into(),
            fixture_path.display().to_string(),
        ),
        ("OPENROUTER_API_KEY".into(), "case-owned-fixture".into()),
        ("AGENT_WORKBENCH_PROTOCOL".into(), protocol.into()),
    ]);
    // A row's successor serves behind its own transport, so a case can hold
    // the successor segment's Start between publication and adoption.
    let successor_proxy = if needs_successor {
        Some(
            V7Proxy::start(
                std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, base + 22))?,
                std::net::SocketAddr::from(([127, 0, 0, 1], base + 21)),
                callback_dir.clone(),
                deadline,
                Vec::new(),
            )
            .await?,
        )
    } else {
        None
    };
    let successor_uri = successor_proxy.as_ref().map(|proxy| proxy.endpoint.clone());
    if let Some(url) = &postgres_url {
        environment.insert("AGENT_WORKBENCH_DATABASE_URL".into(), url.clone());
    }
    let successor = if needs_successor {
        // The existing opt-in shutdown factory adds a real composition
        // declaration. Normal core construction computes and binds a distinct
        // generation from it; no caller supplies a fabricated generation id.
        let mut next_environment = environment.clone();
        next_environment.insert(
            "LASH_HOST_SHUTDOWN_MARKER".into(),
            lease
                .directory
                .join("successor/shutdown.jsonl")
                .display()
                .to_string(),
        );
        next_environment.insert(
            "AGENT_WORKBENCH_DATA_DIR".into(),
            lease.directory.join("workbench-data").display().to_string(),
        );
        if let Some(proxy) = &successor_proxy {
            next_environment.insert(
                "AGENT_WORKBENCH_RESTATE_ADVERTISE_URL".into(),
                proxy.endpoint.clone(),
            );
        }
        Some(
            WorkbenchHost::new(
                boot.nodes[0].ingress_url.clone(),
                boot.nodes[0].admin_url.clone(),
                base + 20,
                base + 21,
            )?
            .configure(next_environment)?,
        )
    } else {
        None
    };
    environment.insert(
        "AGENT_WORKBENCH_RESTATE_ADVERTISE_URL".into(),
        advertised_uri,
    );
    let host = WorkbenchHost::new(
        boot.nodes[0].ingress_url.clone(),
        boot.nodes[0].admin_url.clone(),
        base + 10,
        base + 11,
    )?
    .configure(environment)?;
    let store_root = lease.directory.join("workbench-data/lash-sessions");
    let (stores, postgres): (Arc<dyn lash::StoreSet>, _) = match permutation.store {
        StoreKind::PostgreSql => {
            let storage = lash_postgres_store::PostgresStorage::connect(
                postgres_url
                    .as_deref()
                    .context("PostgreSQL permutation provisioned no database")?,
            )
            .await?;
            (
                Arc::new(lash_postgres_store::PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash::persistence::FileAttachmentStore::new(
                        lease.directory.join("workbench-data/attachments"),
                    )),
                )),
                Some(storage),
            )
        }
        StoreKind::SqliteFile => (
            Arc::new(lash::sqlite::SqliteStoreSet::open(&store_root).await?),
            None,
        ),
        StoreKind::SqliteMemory => bail!("H2 workbench rows need a persistent store"),
    };
    let shared = Arc::new(Shared {
        host: Mutex::new(host),
        successor: Mutex::new(successor),
        generation_drain: Mutex::new(None),
        view: RestateView::new(&boot.nodes[0].admin_url, &lease.namespace)?,
        callbacks: Mutex::new(callbacks),
        directory: callback_dir.clone(),
        delivery,
        deadline,
        row,
        admitted: Mutex::new(None),
        receiver: Mutex::new(None),
        ready: Mutex::new(None),
        proxy: Mutex::new(proxy),
        admin: boot.nodes[0].admin_url.clone(),
        namespace: lease.namespace.clone(),
        stores,
        postgres,
        store_root,
        chat: Mutex::new(None),
        artifacts: vec![server.clone(), artifact.clone()],
        successor_proxy: Mutex::new(successor_proxy),
        successor_uri,
        run_keys: Mutex::new(BTreeMap::new()),
        retained: Mutex::new(Vec::new()),
        predecessor_gone: Mutex::new(false),
        successor_boot: Mutex::new(None),
    });
    let source = shared.clone();
    let snapshot: Snapshot = Arc::new(move |work| {
        let source = source.clone();
        Box::pin(async move { source.journals(&work).await })
    });
    let mut host = Host(shared.clone());
    let mut control = Controller {
        core: CoreControl::new(
            FileBarriers::new(callback_dir, deadline)?,
            Box::new(Reader(snapshot.clone())),
        ),
        shared: shared.clone(),
        observed: Vec::new(),
    };
    let spec = if row.transfers() {
        super::handover::spec(row.id(), permutation.store, vec![server, artifact.clone()])?
    } else if matches!(row, Row::Singleton | Row::Partial | Row::Batch) {
        super::tools::spec(row.id(), permutation.store, vec![server, artifact.clone()])?
    } else {
        super::cancel::spec(row.id(), permutation.store, vec![server, artifact.clone()])?
    };
    ensure!(
        (spec.channel == Channel::Rlm) == (protocol == "rlm"),
        "host protocol differs from case manifest"
    );
    spec.validate()?;
    let mut scenario = Scenario {
        host: &mut host,
        control: &mut control,
        snapshot,
        artifact: &artifact,
        lease: &mut lease,
        ready: None,
        work: None,
        proofs: Vec::new(),
        faults: Vec::new(),
        cancellations: Vec::new(),
    };
    let result = match row {
        Row::Singleton => super::tools::singleton(&mut scenario, &spec).await,
        Row::Partial => super::tools::cold_partial(&mut scenario, &spec).await,
        Row::Batch => super::tools::opposite_order(&mut scenario, &spec).await,
        Row::PreFinal => super::cancel::pre_final(&mut scenario, &spec).await,
        Row::BeforeIntent => {
            super::cancel::post_final(
                &mut scenario,
                &spec,
                super::cancel::ProtectedCut::BeforeIntent,
            )
            .await
        }
        Row::AfterIntent => {
            super::cancel::post_final(
                &mut scenario,
                &spec,
                super::cancel::ProtectedCut::AfterIntent,
            )
            .await
        }
        Row::Ranks => super::cancel::empty_middle_rank(&mut scenario, &spec, permutation.leg).await,
        Row::InlineLoser => super::cancel::live_loser(&mut scenario, &spec, false).await,
        Row::DeferredLoser => super::cancel::live_loser(&mut scenario, &spec, true).await,
        Row::RetirePending => super::handover::retire_pending(&mut scenario, &shared, &spec).await,
        Row::CancelAtCapture => {
            super::handover::cancel_then_fresh(
                &mut scenario,
                &shared,
                &spec,
                super::handover::Cut::CaptureToAdoption,
            )
            .await
        }
        Row::CancelAfterAdoption => {
            super::handover::cancel_then_fresh(
                &mut scenario,
                &shared,
                &spec,
                super::handover::Cut::PostAdoption,
            )
            .await
        }
        Row::PublicationCrash => {
            super::handover::publication_crash(&mut scenario, &shared, &spec).await
        }
        Row::RetainedRemoval => {
            super::handover::remove_retained(&mut scenario, &shared, &spec).await
        }
    };
    let mut errors = Vec::new();
    let mut evidence = match result {
        Ok(evidence) => evidence,
        Err(error) => {
            errors.push(format!("{error:#}"));
            let mut evidence = match scenario.read().await {
                Ok(evidence) => evidence,
                Err(error) => {
                    errors.push(format!("failure snapshot: {error:#}"));
                    Evidence::empty(row.slug().into())
                }
            };
            evidence.barriers.extend(scenario.proofs.clone());
            evidence.faults.extend(scenario.faults.clone());
            evidence
        }
    };
    evidence.artifacts = shared.artifacts.clone();
    if permutation.leg == Leg::Replay && errors.is_empty() {
        // A passing scenario on the replay leg must also show its own Run
        // invocations suspended and resumed on their recorded journal.
        match super::replay::assert_replayed(&shared.directory, &shared.view, &evidence).await {
            Ok(receipt) => evidence.stores.push(receipt),
            Err(error) => errors.push(format!("replay evidence: {error:#}")),
        }
    }
    let host_cleanup = host.stop().await;
    let callback_cleanup = shared.callbacks.lock().await.finish().await;
    let proxy_cleanup = shared.proxy.lock().await.finish().await;
    let successor_proxy_cleanup = match shared.successor_proxy.lock().await.as_mut() {
        Some(proxy) => proxy.finish().await.map(|()| {
            vec![CleanupReceipt {
                resource: format!("listener:{}", base + 22),
                closed: true,
                detail: "successor proxy tasks joined and listener refused connection".into(),
            }]
        }),
        None => Ok(Vec::new()),
    };
    let leg_observation = cluster.observe_leg(&lease.directory).await;
    let cluster_cleanup = cluster.finish().await;
    for (resource, cleanup) in [
        ("host", host_cleanup),
        (
            "body-callback",
            callback_cleanup.map(|()| {
                vec![CleanupReceipt {
                    resource: format!("listener:{}", base + 12),
                    closed: true,
                    detail: "callback tasks joined and listener refused connection".into(),
                }]
            }),
        ),
        (
            "protocol-proxy",
            proxy_cleanup.map(|()| {
                vec![CleanupReceipt {
                    resource: format!("listener:{}", base + 13),
                    closed: true,
                    detail: "proxy tasks joined and listener refused connection".into(),
                }]
            }),
        ),
        ("successor-proxy", successor_proxy_cleanup),
        ("cluster", cluster_cleanup),
    ] {
        match cleanup {
            Ok(receipts) => evidence.cleanup.extend(receipts),
            Err(error) => {
                errors.push(format!("{resource} cleanup: {error:#}"));
                evidence.cleanup.push(CleanupReceipt {
                    resource: resource.into(),
                    closed: false,
                    detail: format!("{error:#}"),
                });
            }
        }
    }
    match leg_observation {
        Ok(receipt) => evidence.stores.push(receipt),
        Err(error) => errors.push(format!("leg observation: {error:#}")),
    }
    if evidence.cleanup.is_empty() || evidence.cleanup.iter().any(|r| !r.closed) {
        errors.push("case leaked owned resources".into());
    }
    super::write(&lease.directory.join("evidence.json"), &evidence)?;
    use lash_upgrade_harness::e2e::evidence::{CaseReceipt, Verdict};
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    let verdict = error
        .as_ref()
        .map_or(Verdict::Passed, |reason| Verdict::Failed {
            reason: reason.clone(),
        });
    let counts = CaseReceipt { evidence, verdict }.write(&lease.directory)?;
    super::write(
        &lease.directory.join("result.json"),
        &json!({"scenario":row.id(),"variant":row.slug(),"selected":counts.selected,
        "executed":counts.executed,"passed":counts.passed,"failed":counts.failed,"not_run":counts.not_run,
        "error":error}),
    )?;
    if let Some(error) = error {
        bail!("{error}");
    }
    counts.reconcile()?;
    println!(
        "H2 {} selected=1 executed=1 passed=1 failed=0 not_run=0",
        row.slug()
    );
    Ok(())
}
