//! Controller wiring for H2. Public admission supplies input/run identities;
//! introspection supplies invocation identities and the original journal bytes.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use lash_upgrade_harness::e2e::{
    Step,
    case::{ArtifactIdentity, CaseLease, Channel, StoreKind},
    cluster::{ClusterControl, LocalCluster},
    control::{
        Barrier, BarrierKind, BarrierProof, CleanupReceipt, Control, CoreControl, Fault,
        FaultReceipt, FileBarriers, ToolControl, WorkIdentity,
        callback::BodyCallbacks,
        transport::{TransportCut, V7Proxy},
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
        }
    }
    fn labels(self) -> &'static [&'static str] {
        match self {
            Self::Singleton => &["echo"],
            Self::Partial => &["a", "b"],
            Self::Batch => &["a", "b", "c"],
            Self::PreFinal | Self::BeforeIntent | Self::AfterIntent => &["intent"],
            Self::Ranks => &["rank_one", "rank_two", "rank_three"],
            Self::InlineLoser | Self::DeferredLoser => &["winner", "loser"],
        }
    }
    fn receiver(self) -> bool {
        matches!(
            self,
            Self::PreFinal | Self::BeforeIntent | Self::AfterIntent | Self::Ranks
        )
    }
}

struct Shared {
    host: Mutex<WorkbenchHost>,
    view: RestateView,
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
    store_root: PathBuf,
    chat: Mutex<Option<String>>,
    artifacts: Vec<ArtifactIdentity>,
}
impl Shared {
    async fn journals(&self, work: &WorkIdentity) -> Result<Evidence> {
        #[derive(serde::Deserialize)]
        struct Invocation {
            id: String,
            pinned_service_protocol_version: Option<u32>,
        }
        // A case owns one chat and no other turn. Include every actual segment
        // after handover, retaining each invocation's own journal provenance.
        let prefix = self.view.service_name("LashTurn").replace('\'', "''");
        let rows: Vec<Invocation> = self.view.query(&format!(
            "SELECT id, pinned_service_protocol_version FROM sys_invocation WHERE target_service_name LIKE '{prefix}%' AND target_handler_name = 'run' ORDER BY created_at"
        )).await?;
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
            let mut segment = work.clone();
            segment.segment = row.id.clone();
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
    async fn retained_cancel(&self, work: &WorkIdentity, evidence: &mut Evidence) -> Result<()> {
        use lash_core::store::TurnInputStore as _;
        let chat = self
            .chat
            .lock()
            .await
            .clone()
            .context("no bound native session")?;
        let session = lash::SessionId::parse(chat)?;
        let run = lash::TurnId::parse(&work.run)?;
        let stores = lash::sqlite::SqliteStoreSet::open(&self.store_root).await?;
        let store = stores.open_store().await?;
        let address = lash::TurnAddress::new(session, run);
        if let Some(record) = store.turn_cancel_request(&address).await? {
            let artifact = self.directory.join("native-cancel-request.json");
            super::write(&artifact, &record)?;
            evidence.stores.push(json!({"kind":"h2_native_cancel_request",
                "artifact":artifact,"store":self.store_root.join("durable-core.db"),"record":record}));
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
        let path = self.store_root.join("durable-core.db");
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String,String)>> {
            let db = rusqlite::Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let mut query = db.prepare("SELECT turn_id,result_json FROM runtime_turn_commits WHERE session_id=?1 ORDER BY change_seq")?;
            Ok(query.query_map([chat], |row| Ok((row.get(0)?,row.get(1)?)))?.collect::<std::result::Result<_,_>>()?)
        }).await??;
        for (turn, raw) in rows {
            let receipt = lash_core::store::decode_runtime_commit_receipt(&session, &turn, &raw)?;
            if let Some(follow_on) = &receipt.pending_follow_on {
                if follow_on
                    .continuation
                    .as_ref()
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
        }
        Ok(())
    }
    async fn bind(&self, work: WorkIdentity, chat: &str) -> Result<WorkIdentity> {
        use lash_core::store::RunStore as _;
        let stores = lash::sqlite::SqliteStoreSet::open(&self.store_root).await?;
        let store = stores.open_store().await?;
        let session = lash::SessionId::parse(chat)?;
        let run = lash::TurnId::parse(&work.run)?;
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
            *self.0.ready.lock().await = Some(ready.clone());
            Ok(ready)
        })
    }
    fn command<'a>(&'a mut self, command: HostCommand) -> Step<'a, HostObservation> {
        Box::pin(async move {
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
        Box::pin(async move { self.0.host.lock().await.stop().await })
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
                    ensure!(
                        Instant::now() < self.shared.deadline,
                        "continuation publication was absent"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
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
                self.shared
                    .host
                    .lock()
                    .await
                    .command(HostCommand::Process {
                        action: "resolve".into(),
                        input: json!({"key":key,"value":value}),
                    })
                    .await?;
                Ok(())
            } else {
                if let ToolControl::Hold(barrier) = &command {
                    if matches!(
                        barrier.kind,
                        BarrierKind::DeclarationIssued | BarrierKind::VProposed
                    ) {
                        self.shared.proxy.lock().await.arm_cut(TransportCut {
                            proposal: barrier.clone(),
                            before_ack: Barrier {
                                work: barrier.work.clone(),
                                kind: BarrierKind::BeforeAck,
                            },
                        })?;
                    }
                }
                self.core.tool(command).await
            }
        })
    }
}

pub async fn run(row: Row) -> Result<()> {
    let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
    std::fs::create_dir_all(&root)?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut lease = CaseLease::new(row.slug(), root.join(row.slug()), deadline)?;
    let base: u16 = std::env::var("LASH_E2E_PORT_BASE")?.parse()?;
    ensure!(base <= u16::MAX - 50, "private port range overflow");
    lease.ports = (base + 10..base + 14).collect();
    let server = super::artifact(
        "restate-server",
        std::env::var("LASH_RESTATE_SERVER_BIN")?.into(),
    )?;
    let artifact = super::artifact(
        "agent-workbench",
        std::env::var("LASH_WORKBENCH_E2E_BIN")?.into(),
    )?;
    let mut cluster = LocalCluster::new(base, deadline);
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
        "deferred_loser":matches!(row,Row::DeferredLoser)});
    let protocol = if matches!(row, Row::Ranks | Row::InlineLoser | Row::DeferredLoser) {
        "rlm"
    } else {
        "standard"
    };
    let fixture_path = lease.directory.join("tool-fixture.json");
    super::write(&fixture_path, &fixture)?;
    let host = WorkbenchHost::new(
        boot.nodes[0].ingress_url.clone(),
        boot.nodes[0].admin_url.clone(),
        base + 10,
        base + 11,
    )?
    .configure(BTreeMap::from([
        (
            "AGENT_WORKBENCH_TOOL_FIXTURE".into(),
            fixture_path.display().to_string(),
        ),
        ("OPENROUTER_API_KEY".into(), "case-owned-fixture".into()),
        ("AGENT_WORKBENCH_PROTOCOL".into(), protocol.into()),
        (
            "AGENT_WORKBENCH_RESTATE_ADVERTISE_URL".into(),
            advertised_uri,
        ),
    ]))?;
    let shared = Arc::new(Shared {
        host: Mutex::new(host),
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
        store_root: lease.directory.join("workbench-data/lash-sessions"),
        chat: Mutex::new(None),
        artifacts: vec![server.clone(), artifact.clone()],
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
    let spec = if matches!(row, Row::Singleton | Row::Partial | Row::Batch) {
        super::tools::spec(
            row.id(),
            StoreKind::SqliteFile,
            vec![server, artifact.clone()],
        )?
    } else {
        super::cancel::spec(
            row.id(),
            StoreKind::SqliteFile,
            vec![server, artifact.clone()],
        )?
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
        Row::Ranks => super::cancel::empty_middle_rank(&mut scenario, &spec).await,
        Row::InlineLoser => super::cancel::live_loser(&mut scenario, &spec, false).await,
        Row::DeferredLoser => super::cancel::live_loser(&mut scenario, &spec, true).await,
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
    let host_cleanup = host.stop().await;
    let callback_cleanup = shared.callbacks.lock().await.finish().await;
    let proxy_cleanup = shared.proxy.lock().await.finish().await;
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
