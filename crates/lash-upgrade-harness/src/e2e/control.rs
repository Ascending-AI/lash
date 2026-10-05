//! Out-of-journal controls. A proposed record is never a durable barrier.
use super::Step;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkIdentity {
    pub ingress: String,
    pub run: String,
    pub segment: String,
    pub call: Option<String>,
    pub ordinal: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BarrierKind {
    AdmissionDurable,
    RunCancelRecorded,
    RetryBackoffEntered,
    RetryScheduleDurable,
    PublicationRequest,
    PublicationRefused,
    SuccessorFence,
    BodyEntered,
    SideEffectAccepted,
    XProposed,
    XDurable,
    DProposed,
    DDurable,
    DeclarationIssued,
    VProposed,
    VDurable,
    ContinuationPublished,
    SuccessorAdmitted,
    PredecessorDischarged,
    SourceSealed,
    Suspended,
    HostReady,
    TransportConnected,
    BeforeAck,
    WorkerReaped,
    StartAdmitted,
    StartRegistered,
    ConsumerHoldDischarged,
    ParkCommitted,
    TelemetryFlushed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Barrier {
    pub work: WorkIdentity,
    pub kind: BarrierKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BarrierProof {
    pub barrier: Barrier,
    /// Real admin/journal artifact or independently durable outside-effect receipt.
    pub artifact: String,
    pub journal_index: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Fault {
    KillHost { target: String },
    KillVm { target: String },
    RestartRestate { node: u32 },
    KillRestate { node: u32 },
    DropConnection { target: String },
    PartitionLink { from: u32, to: u32 },
    HealLink { from: u32, to: u32 },
    Redeploy { target: String, artifact: String },
    DrainAndRetire { generation: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaultReceipt {
    pub fault: Fault,
    pub proof: BarrierProof,
    pub target_incarnation: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessReceipt {
    pub role: String,
    pub pid: u32,
    pub incarnation: u32,
    pub log: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupReceipt {
    pub resource: String,
    pub closed: bool,
    pub detail: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolControl {
    Hold(Barrier),
    Release(Barrier),
    Resolve {
        work: WorkIdentity,
        value: serde_json::Value,
    },
}
pub trait Control {
    fn await_barrier<'a>(&'a mut self, barrier: &'a Barrier) -> Step<'a, BarrierProof>;
    fn inject<'a>(&'a mut self, fault: Fault, proof: &'a BarrierProof) -> Step<'a, FaultReceipt>;
    fn tool<'a>(&'a mut self, command: ToolControl) -> Step<'a, ()>;
}

/// File control is deliberately outside the invocation journal. Durable phases
/// are published by an evidence reader, never by a tool body reporting success.
pub struct FileBarriers {
    directory: std::path::PathBuf,
    deadline: std::time::Instant,
}
impl FileBarriers {
    pub fn new(
        directory: std::path::PathBuf,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&directory)?;
        Ok(Self {
            directory,
            deadline,
        })
    }
    fn path(&self, barrier: &Barrier, suffix: &str) -> anyhow::Result<std::path::PathBuf> {
        let key = lash_core::stable_hash::sha256_hex(&serde_json::to_vec(barrier)?);
        Ok(self.directory.join(format!("{key}.{suffix}")))
    }
    pub fn hold(&self, barrier: &Barrier) -> anyhow::Result<()> {
        crate::node::write_atomically(&self.path(barrier, "hold")?, &serde_json::to_vec(barrier)?)
    }
    pub fn release(&self, barrier: &Barrier) -> anyhow::Result<()> {
        crate::node::write_atomically(
            &self.path(barrier, "release")?,
            &serde_json::to_vec(barrier)?,
        )
    }
    pub fn publish(&self, proof: &BarrierProof) -> anyhow::Result<()> {
        anyhow::ensure!(
            !proof.artifact.is_empty(),
            "barrier lacks evidence provenance"
        );
        anyhow::ensure!(
            !proof.barrier.kind.journal_backed() || proof.journal_index.is_some(),
            "durable barrier lacks decoded journal index"
        );
        if proof.barrier.kind.durable() && !proof.barrier.kind.journal_backed() {
            let bytes = std::fs::read(&proof.artifact)?;
            anyhow::ensure!(
                serde_json::from_slice::<serde_json::Value>(&bytes)?.is_object(),
                "store barrier lacks an independently read JSON receipt"
            );
        }
        crate::node::write_atomically(
            &self.path(&proof.barrier, "reached")?,
            &serde_json::to_vec(proof)?,
        )
    }
    /// Adapter oracles publish independently read fenced-store facts here.
    /// A store revision/fence is never relabelled as a V7 journal index.
    pub fn publish_store(
        &self,
        barrier: &Barrier,
        artifact: &std::path::Path,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            barrier.kind.durable() && !barrier.kind.journal_backed(),
            "phase requires actual decoded journal provenance"
        );
        let bytes = std::fs::read(artifact)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            value.is_object(),
            "independent store receipt is not an object"
        );
        self.publish(&BarrierProof {
            barrier: barrier.clone(),
            artifact: artifact.display().to_string(),
            journal_index: None,
        })
    }
    pub async fn await_proof(&self, barrier: &Barrier) -> anyhow::Result<BarrierProof> {
        loop {
            match std::fs::read(self.path(barrier, "reached")?) {
                Ok(bytes) => {
                    let proof: BarrierProof = serde_json::from_slice(&bytes)?;
                    anyhow::ensure!(proof.barrier == *barrier, "barrier identity mismatch");
                    anyhow::ensure!(
                        !barrier.kind.journal_backed() || proof.journal_index.is_some(),
                        "durable barrier lacks journal proof"
                    );
                    return Ok(proof);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            anyhow::ensure!(
                std::time::Instant::now() < self.deadline,
                "barrier {:?} was missed",
                barrier
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
    /// Tool bodies publish only body/transport facts and then await an explicit release.
    pub async fn enter(&self, barrier: &Barrier, artifact: String) -> anyhow::Result<()> {
        anyhow::ensure!(
            !barrier.kind.durable(),
            "body cannot certify journal durability"
        );
        self.publish(&BarrierProof {
            barrier: barrier.clone(),
            artifact,
            journal_index: None,
        })?;
        self.await_release(barrier).await
    }
    pub async fn await_release(&self, barrier: &Barrier) -> anyhow::Result<()> {
        while self.path(barrier, "hold")?.exists() && !self.path(barrier, "release")?.exists() {
            anyhow::ensure!(
                std::time::Instant::now() < self.deadline,
                "held barrier was not released"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        Ok(())
    }
}
impl BarrierKind {
    pub fn durable(&self) -> bool {
        self.journal_backed()
            || matches!(
                self,
                Self::ContinuationPublished
                    | Self::SuccessorAdmitted
                    | Self::SourceSealed
                    | Self::ParkCommitted
                    | Self::SuccessorFence
                    | Self::PublicationRefused
            )
    }
    pub fn journal_backed(&self) -> bool {
        matches!(
            self,
            Self::AdmissionDurable
                | Self::XDurable
                | Self::DDurable
                | Self::VDurable
                | Self::RunCancelRecorded
                | Self::StartAdmitted
                | Self::StartRegistered
                | Self::ConsumerHoldDischarged
                | Self::RetryScheduleDurable
        )
    }
}

pub mod callback;
pub mod process;
pub mod transport;

/// Shared controller for real owned child handles, identity-keyed fixture
/// gates and journal-backed durable cuts. Product controls remain adapters.
pub struct CoreControl {
    pub barriers: FileBarriers,
    pub reader: Box<dyn super::evidence::EvidenceReader + Send>,
    pub cluster: Option<super::cluster::LocalCluster>,
    pub host: Option<Box<dyn super::host::HostAdapter + Send>>,
    processes: std::collections::BTreeMap<String, (u32, crate::harness::ServingNode)>,
    proxies: std::collections::BTreeMap<String, (u32, transport::V7Proxy)>,
    observed: Vec<BarrierProof>,
    pub receipts: Vec<FaultReceipt>,
}
impl CoreControl {
    pub fn new(
        barriers: FileBarriers,
        reader: Box<dyn super::evidence::EvidenceReader + Send>,
    ) -> Self {
        Self {
            barriers,
            reader,
            cluster: None,
            host: None,
            processes: Default::default(),
            proxies: Default::default(),
            observed: Vec::new(),
            receipts: Vec::new(),
        }
    }
    pub fn own_process(
        &mut self,
        target: String,
        incarnation: u32,
        process: crate::harness::ServingNode,
    ) -> anyhow::Result<()> {
        if let Some((previous, owned)) = self.processes.get(&target) {
            anyhow::ensure!(
                owned.is_reaped() && incarnation == previous + 1,
                "replacement must follow a reaped predecessor with the next incarnation"
            );
        }
        self.processes.insert(target, (incarnation, process));
        Ok(())
    }
    pub async fn wait_process_success(
        &mut self,
        target: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        self.processes
            .get_mut(target)
            .ok_or_else(|| anyhow::anyhow!("process target is not owned"))?
            .1
            .wait_success(deadline)
            .await
    }
    pub fn own_proxy(
        &mut self,
        target: String,
        incarnation: u32,
        proxy: transport::V7Proxy,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.proxies.contains_key(&target),
            "transport target is already owned"
        );
        self.proxies.insert(target, (incarnation, proxy));
        Ok(())
    }
    pub fn proxy(&self, target: &str) -> anyhow::Result<&transport::V7Proxy> {
        self.proxies
            .get(target)
            .map(|(_, proxy)| proxy)
            .ok_or_else(|| anyhow::anyhow!("transport target is not owned"))
    }
    /// Take an owned process back out for an orderly teardown stop.
    pub fn take_process(&mut self, target: &str) -> anyhow::Result<crate::harness::ServingNode> {
        self.processes
            .remove(target)
            .map(|(_, process)| process)
            .ok_or_else(|| anyhow::anyhow!("process target is not owned"))
    }
    /// Take an owned transport target back out for an orderly finish.
    pub fn take_proxy(&mut self, target: &str) -> anyhow::Result<transport::V7Proxy> {
        self.proxies
            .remove(target)
            .map(|(_, proxy)| proxy)
            .ok_or_else(|| anyhow::anyhow!("transport target is not owned"))
    }
}
impl Control for CoreControl {
    fn await_barrier<'a>(&'a mut self, barrier: &'a Barrier) -> Step<'a, BarrierProof> {
        Box::pin(async move {
            let proof = if barrier.kind.journal_backed() {
                loop {
                    for (_, process) in self.processes.values_mut() {
                        if !process.is_reaped() {
                            process.assert_running()?;
                        }
                    }
                    let evidence = self.reader.collect(&barrier.work).await?;
                    if let Some(fact) = evidence
                        .journals
                        .iter()
                        .find(|fact| journal_matches(fact, barrier))
                    {
                        let path = self.barriers.path(barrier, "journal.json")?;
                        crate::node::write_atomically(&path, &serde_json::to_vec(fact)?)?;
                        let proof = BarrierProof {
                            barrier: barrier.clone(),
                            artifact: path.display().to_string(),
                            journal_index: Some(fact.index),
                        };
                        self.barriers.publish(&proof)?;
                        break proof;
                    }
                    anyhow::ensure!(
                        std::time::Instant::now() < self.barriers.deadline,
                        "durable barrier was missed: {barrier:?}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            } else {
                self.barriers.await_proof(barrier).await?
            };
            self.observed.push(proof.clone());
            Ok(proof)
        })
    }
    fn inject<'a>(&'a mut self, fault: Fault, proof: &'a BarrierProof) -> Step<'a, FaultReceipt> {
        Box::pin(async move {
            use super::cluster::ClusterControl;
            anyhow::ensure!(
                self.observed
                    .iter()
                    .any(|observed| observed.barrier == proof.barrier
                        && observed.artifact == proof.artifact
                        && observed.journal_index == proof.journal_index),
                "fault was not armed by an observed exact barrier"
            );
            let incarnation = match &fault {
                Fault::KillHost { target } | Fault::KillVm { target } => {
                    let (incarnation, process) = self
                        .processes
                        .get_mut(target)
                        .ok_or_else(|| anyhow::anyhow!("fault targets an unowned process"))?;
                    process.kill_and_reap()?;
                    *incarnation
                }
                Fault::DropConnection { target } => {
                    let (incarnation, proxy) = self
                        .proxies
                        .get(target)
                        .ok_or_else(|| anyhow::anyhow!("transport target is not owned"))?;
                    let count = proxy.disconnect(self.barriers.deadline).await?;
                    let path = self.barriers.directory.join(format!(
                        "disconnect-{}.json",
                        lash_core::stable_hash::sha256_hex(target.as_bytes())
                    ));
                    crate::node::write_atomically(
                        &path,
                        &serde_json::to_vec(
                            &serde_json::json!({"target":target,"closed_streams":count,"incarnation":incarnation,"barrier":proof}),
                        )?,
                    )?;
                    *incarnation
                }
                Fault::KillRestate { node } => {
                    let receipt = self
                        .cluster
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("no cluster is owned"))?
                        .kill(*node, proof)
                        .await?;
                    receipt.target_incarnation
                }
                Fault::RestartRestate { node } => {
                    let cluster = self
                        .cluster
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("no cluster is owned"))?;
                    let receipt = cluster.kill(*node, proof).await?;
                    cluster.restart(*node).await?;
                    receipt.target_incarnation
                }
                Fault::PartitionLink { from, to } => {
                    self.cluster
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("no cluster is owned"))?
                        .partition(*from, *to)
                        .await?;
                    0
                }
                Fault::HealLink { from, to } => {
                    self.cluster
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("no cluster is owned"))?
                        .heal(*from, *to)
                        .await?;
                    0
                }
                _ => anyhow::bail!("fault requires a registered product adapter: {fault:?}"),
            };
            let receipt = FaultReceipt {
                fault,
                proof: proof.clone(),
                target_incarnation: incarnation,
            };
            self.receipts.push(receipt.clone());
            Ok(receipt)
        })
    }
    fn tool<'a>(&'a mut self, command: ToolControl) -> Step<'a, ()> {
        Box::pin(async move {
            match command {
                ToolControl::Hold(barrier) => self.barriers.hold(&barrier),
                ToolControl::Release(barrier) => self.barriers.release(&barrier),
                ToolControl::Resolve { work, value } => {
                    let host = self.host.as_mut().ok_or_else(|| {
                        anyhow::anyhow!("source resolution needs a registered host adapter")
                    })?;
                    host.command(super::host::HostCommand::Process {
                        action: "resolve-source".into(),
                        input: serde_json::json!({"work":work,"value":value}),
                    })
                    .await?;
                    Ok(())
                }
            }
        })
    }
}
fn journal_matches(fact: &super::evidence::JournalFact, barrier: &Barrier) -> bool {
    use super::evidence::DecodedRecord;
    use lash_core_store::tool_run::RunEvent;
    if fact.work != barrier.work {
        return false;
    }
    let call_matches =
        |call: &lash_core::ToolCallId| barrier.work.call.as_deref() == Some(call.as_str());
    match &fact.decoded {
        Some(DecodedRecord::Attempt(attempt)) => {
            barrier.kind == BarrierKind::XDurable
                && call_matches(&attempt.call_id)
                && barrier.work.ordinal == Some(attempt.attempt.get())
        }
        Some(DecodedRecord::Run(entry)) => entry.record.events.iter().any(|event| match event {
            RunEvent::StartAdmitted { call_id, .. } => {
                barrier.kind == BarrierKind::StartAdmitted && call_matches(call_id)
            }
            RunEvent::StartLaunched { call_id, .. } => {
                barrier.kind == BarrierKind::StartRegistered && call_matches(call_id)
            }
            RunEvent::StartDischarged { call_id, .. } => {
                barrier.kind == BarrierKind::ConsumerHoldDischarged && call_matches(call_id)
            }
            RunEvent::Lifecycle { state } => {
                barrier.kind == BarrierKind::RunCancelRecorded
                    && *state == lash_core_store::tool_run::RunLifecycle::Closing
            }
            RunEvent::Admitted { round } => {
                barrier.kind == BarrierKind::AdmissionDurable
                    && (barrier.work.call.is_none()
                        || round
                            .members
                            .iter()
                            .any(|member| call_matches(&member.call_id)))
            }
            RunEvent::AttemptRecorded {
                call_id, attempt, ..
            } => {
                barrier.kind == BarrierKind::XDurable
                    && call_matches(call_id)
                    && barrier.work.ordinal == Some(attempt.get())
            }
            RunEvent::Decided { call_id, .. } => {
                barrier.kind == BarrierKind::DDurable && call_matches(call_id)
            }
            RunEvent::Presented { call_id, .. } => {
                barrier.kind == BarrierKind::VDurable && call_matches(call_id)
            }
            RunEvent::RetryScheduled {
                call_id, failed, ..
            }
            | RunEvent::RetryTimerRegistered {
                call_id, failed, ..
            } => {
                barrier.kind == BarrierKind::RetryScheduleDurable
                    && call_matches(call_id)
                    && barrier.work.ordinal == Some(failed.get())
            }
            _ => false,
        }),
        Some(DecodedRecord::Transfer(_)) => barrier.kind == BarrierKind::ContinuationPublished,
        None => false,
    }
}
