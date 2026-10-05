//! S19/S20 (L08) on the agent workbench, the product host. An RLM cell calls
//! a tool declared isolated; the production route binds it at admission to
//! the fixture's registered `WorkerProcessEngine`, and every OS worker the
//! engine spawns writes its own PID marker.
//!
//! Each leg is its own session and Run. The scripted model's first answer is
//! held until the case has bound the Run's invocation and armed its cuts;
//! cuts are holds on that invocation's own proposals and ACKs, and
//! durability is read from its decoded journal. No sleep establishes
//! readiness or completion.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use lash_core::tool_dispatch::{IsolatedProcessDescriptor, ProcessExecutionBoundary};
use lash_core::tool_run::RunEvent;
use lash_remote_protocol::{RemoteToolCallOutcome, RemoteTurnReport, RemoteTurnStatus};
use lash_upgrade_harness::e2e::{
    case::CaseLease,
    cluster::{ClusterControl, LocalCluster},
    control::{
        Barrier, BarrierKind, BarrierProof, CleanupReceipt, Fault, FaultReceipt, FileBarriers,
        WorkIdentity,
        transport::{TransportCut, V7Proxy},
    },
    evidence::{CaseReceipt, DecodedRecord, Evidence, JournalFact, RestateEvidenceReader, Verdict},
    host::{HostAdapter, HostCommand, HostReady},
    host_adapters::workbench::WorkbenchHost,
};
use lash_upgrade_harness::restate_view::RestateView;
use serde_json::{Value, json};

const CASE_DEADLINE: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_millis(25);
const ISOLATED_KEY: &str = "process-start-key:v1:isolated:";

/// L08: an unbound isolated tool refuses typed before any body; host SIGKILL
/// between durable admission and launch, and between registration and the
/// send of its launch record, recovers the same StartKey and one admitted
/// process identity on the product route; no ordinary body ever runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s19_isolated_start_cuts_recover_one_process_on_the_workbench() -> Result<()> {
    let mut case = Case::boot("s19-workbench-isolated", "S19").await?;
    let result = s19(&mut case).await;
    case.finish("S19", result).await
}

/// L08: cancellation before admission forbids the start; cancellation after
/// admission launches under the admitted key, then terminates and reaps the
/// worker before the hold is released; a workbench killed after the worker's
/// death but before the discharge is durable recovers the same receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s20_isolated_cancel_terminates_and_reaps_worker_on_the_workbench() -> Result<()> {
    let mut case = Case::boot("s20-workbench-isolated", "S20").await?;
    let result = s20(&mut case).await;
    case.finish("S20", result).await
}

async fn s19(case: &mut Case) -> Result<()> {
    let leg = case.submit("unbound", &[]).await?;
    let report = case.settled(&leg).await?;
    let record = report
        .tool_calls
        .iter()
        .find(|record| record.tool_name.contains("unbound"))
        .with_context(|| format!("the turn records no unbound call: {report:?}"))?;
    let RemoteToolCallOutcome::Failure(failure) = &record.output.outcome else {
        bail!("the unbound call was not refused: {record:?}");
    };
    ensure!(
        failure.code == lash_core::ToolAdmissionRefusal::CODE
            && failure.cause.as_deref()
                == Some(&lash_core::ToolFailureCause::Admission {
                    refusal: lash_core::ToolAdmissionRefusal::UnsupportedIsolation,
                }),
        "the unbound call refused untyped: {failure:?}"
    );
    case.assert_never_started(&leg).await?;
    case.evidence
        .effects
        .push(json!({"kind":"s19_unbound_refusal","run":leg.work.run,"record":record}));

    let leg = case
        .submit("admission", &[BarrierKind::DeclarationIssued])
        .await?;
    case.pass(&leg, BarrierKind::DeclarationIssued).await?;
    case.hold(&leg, BarrierKind::BeforeAck).await?;
    let admitted = case.await_start(&leg, BarrierKind::StartAdmitted).await?;
    ensure!(
        !case.events(&leg).await?.iter().any(is_launched),
        "launch was durable before the admission cut"
    );
    ensure!(
        case.spawned(&leg)?.is_empty() && case.rows_for(&leg).await?.is_empty(),
        "a worker or process row exists before launch"
    );
    case.kill(admitted).await?;
    case.release(&leg, BarrierKind::BeforeAck)?;
    case.restart().await?;
    let report = case.settled(&leg).await?;
    ensure!(report.status() == RemoteTurnStatus::Answered, "{report:?}");
    let descriptor = presented_descriptor(&report)?;
    case.assert_recovered(&leg, &descriptor, None).await?;

    let leg = case.submit("registered", &[BarrierKind::VProposed]).await?;
    let held = case.hold(&leg, BarrierKind::VProposed).await?;
    let rows = case.rows_for(&leg).await?;
    ensure!(rows.len() == 1, "registration left {} rows", rows.len());
    let process = rows[0].process_id.clone();
    let pid = case.await_spawn(&leg).await?;
    ensure!(alive(pid), "the registered worker is not running");
    ensure!(
        parent(pid) == Some(case.ready.process.pid),
        "the worker is not a child of the serving workbench"
    );
    ensure!(
        !case.events(&leg).await?.iter().any(is_launched),
        "the launch record was durable before the registration cut"
    );
    case.kill(held).await?;
    ensure!(
        alive(pid),
        "the worker did not outlive its killed workbench"
    );
    case.release(&leg, BarrierKind::VProposed)?;
    case.release(&leg, BarrierKind::BeforeAck)?;
    case.restart().await?;
    let report = case.settled(&leg).await?;
    ensure!(report.status() == RemoteTurnStatus::Answered, "{report:?}");
    let descriptor = presented_descriptor(&report)?;
    case.assert_recovered(&leg, &descriptor, Some((&process, pid)))
        .await
}

async fn s20(case: &mut Case) -> Result<()> {
    let leg = case
        .submit("pre-admission", &[BarrierKind::XProposed])
        .await?;
    case.hold(&leg, BarrierKind::XProposed).await?;
    let cancel = case.cancel(&leg).await?;
    case.release(&leg, BarrierKind::XProposed)?;
    case.release(&leg, BarrierKind::BeforeAck)?;
    let report = case.settled(&leg).await?;
    ensure!(
        report.status() == RemoteTurnStatus::Cancelled,
        "a cancel before admission did not cancel the Run: {:?}",
        report.outcome
    );
    case.assert_never_started(&leg).await?;
    case.evidence.effects.push(
        json!({"kind":"s20_pre_admission_cancel","run":leg.work.run,"cancel":cancel,
            "outcome":report.outcome}),
    );

    let leg = case
        .submit(
            "cancel",
            &[BarrierKind::DeclarationIssued, BarrierKind::VProposed],
        )
        .await?;
    case.pass(&leg, BarrierKind::DeclarationIssued).await?;
    case.hold(&leg, BarrierKind::BeforeAck).await?;
    case.await_start(&leg, BarrierKind::StartAdmitted).await?;
    let cancel = case.cancel(&leg).await?;
    ensure!(
        case.spawned(&leg)?.is_empty() && case.rows_for(&leg).await?.is_empty(),
        "a cancel request alone started or released anything: no launch yet"
    );
    case.evidence
        .stores
        .push(json!({"kind":"s20_cancel_after_admission","run":leg.work.run,"cancel":cancel}));
    case.release(&leg, BarrierKind::BeforeAck)?;
    let held = case.hold(&leg, BarrierKind::VProposed).await?;
    let pids = case.spawned(&leg)?;
    let [pid] = pids.as_slice() else {
        bail!("the admitted start spawned {pids:?}, not one worker");
    };
    let pid = *pid;
    ensure!(
        gone(pid),
        "the terminated worker {pid} was not reaped before discharge"
    );
    let rows = case.rows_for(&leg).await?;
    ensure!(
        rows.len() == 1 && rows[0].hold.is_none() && rows[0].cancel_requested,
        "the discharge did not cancel and release after termination: {rows:?}"
    );
    let process = rows[0].process_id.clone();
    let proposed = proposed_discharge(&held)?;
    ensure!(
        proposed.termination.as_ref().is_some_and(|receipt| {
            receipt.process_id.to_string() == process && receipt.worker_pid.get() == pid
        }),
        "the proposed descriptor names another termination: {proposed:?}"
    );
    ensure!(
        !case
            .events(&leg)
            .await?
            .iter()
            .any(|event| matches!(event, RunEvent::StartDischarged { .. })),
        "the discharge was durable before the workbench cut"
    );
    case.evidence
        .stores
        .push(json!({"kind":"s20_before_discharge_ack","rows":rows,
        "worker_pid":pid,"reaped":true,"proposed":proposed}));
    case.kill(held).await?;
    case.release(&leg, BarrierKind::VProposed)?;
    case.restart().await?;
    let report = case.settled(&leg).await?;
    let descriptor = presented_descriptor(&report)?;
    let starts = case.starts(&leg).await?;
    ensure!(
        matches!(
            starts.as_slice(),
            [
                RunEvent::StartAdmitted { .. },
                RunEvent::StartLaunched { process_id, .. },
                RunEvent::StartDischarged { cancelled: true, .. },
            ] if process_id.to_string() == process
        ),
        "the cancelled start did not record admit/launch/discharge(cancelled) once: {starts:?}"
    );
    ensure!(
        descriptor.process_id.to_string() == process
            && descriptor.boundary == ProcessExecutionBoundary::WorkerProcess
            && descriptor.termination == proposed.termination,
        "recovery lost the recorded termination receipt: {descriptor:?}"
    );
    ensure!(
        case.spawned(&leg)? == vec![pid],
        "recovery spawned a replacement worker"
    );
    let rows = case.rows_for(&leg).await?;
    ensure!(
        rows.len() == 1 && rows[0].hold.is_none(),
        "discharge left the registry at {rows:?}"
    );
    ensure!(case.deliveries()? == 0, "an ordinary body ran");
    Ok(())
}

fn is_start(event: &RunEvent) -> bool {
    matches!(
        event,
        RunEvent::StartAdmitted { .. }
            | RunEvent::StartLaunched { .. }
            | RunEvent::StartDischarged { .. }
    )
}

fn is_launched(event: &RunEvent) -> bool {
    matches!(event, RunEvent::StartLaunched { .. })
}

/// The descriptor the isolated call presented, from the settled report.
fn presented_descriptor(report: &RemoteTurnReport) -> Result<IsolatedProcessDescriptor> {
    let record = report
        .tool_calls
        .iter()
        .find(|record| record.tool_name.contains("isolated"))
        .with_context(|| format!("the turn records no isolated call: {report:?}"))?;
    let RemoteToolCallOutcome::Success(value) = &record.output.outcome else {
        bail!("the isolated call presented no descriptor: {record:?}");
    };
    let value = match value {
        Value::String(text) => serde_json::from_str(text)?,
        value => value.clone(),
    };
    serde_json::from_value(value)
        .with_context(|| format!("the isolated call presented no descriptor: {record:?}"))
}

/// The descriptor a held V proposal carries, decoded from its wire frame.
fn proposed_discharge(proof: &BarrierProof) -> Result<IsolatedProcessDescriptor> {
    use lash_restate_test::protocol::generated::{
        ProposeRunCompletionMessage, propose_run_completion_message,
    };
    use prost::Message as _;
    let frame: Value = serde_json::from_slice(&std::fs::read(&proof.artifact)?)?;
    let payload: Vec<u8> = serde_json::from_value(frame["payload"].clone())?;
    let message = ProposeRunCompletionMessage::decode(payload.as_slice())?;
    let Some(propose_run_completion_message::Result::Value(bytes)) = message.result else {
        bail!("the held V proposal carries no value");
    };
    let value: Value = serde_json::from_slice(&bytes)?;
    find_descriptor(&value)
        .with_context(|| format!("the held V proposal has no descriptor: {value}"))
}

fn find_descriptor(value: &Value) -> Option<IsolatedProcessDescriptor> {
    if let Ok(descriptor) = serde_json::from_value::<IsolatedProcessDescriptor>(value.clone()) {
        return Some(descriptor);
    }
    match value {
        Value::Object(fields) => fields.values().find_map(find_descriptor),
        Value::Array(values) => values.iter().find_map(find_descriptor),
        Value::String(text) if text.starts_with('{') => serde_json::from_str::<Value>(text)
            .ok()
            .as_ref()
            .and_then(find_descriptor),
        _ => None,
    }
}

/// One process row as read back from the workbench's registry file.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct Row {
    process_id: String,
    start_key: String,
    hold: Option<String>,
    cancel_requested: bool,
}

/// One leg's Run: its session and the admitted work bound to its invocation.
struct Leg {
    session: String,
    work: WorkIdentity,
    /// Worker markers written before this leg's Run started.
    spawned_before: usize,
}

struct Case {
    slug: &'static str,
    scenario: &'static str,
    lease: CaseLease,
    cluster: LocalCluster,
    host: WorkbenchHost,
    ready: HostReady,
    proxy: V7Proxy,
    barriers: FileBarriers,
    reader: RestateEvidenceReader,
    view: RestateView,
    store_root: PathBuf,
    delivery: PathBuf,
    marker: PathBuf,
    base: u16,
    evidence: Evidence,
    /// Every isolated start key a finished leg owned, so a later leg's rows
    /// are only its own.
    seen: BTreeSet<String>,
    /// The process groups of killed workbench incarnations: what they spawned
    /// outlived them, and the case closes it at the end.
    killed: Vec<u32>,
}

impl Case {
    async fn boot(slug: &'static str, scenario: &'static str) -> Result<Self> {
        let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
        std::fs::create_dir_all(&root)?;
        let deadline = Instant::now() + CASE_DEADLINE;
        let mut lease = CaseLease::new(slug, root.join(slug), deadline)?;
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
        let barrier_dir = lease.directory.join("barriers");
        std::fs::create_dir_all(&barrier_dir)?;
        let proxy = V7Proxy::start(
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, base + 13))?,
            std::net::SocketAddr::from(([127, 0, 0, 1], base + 11)),
            barrier_dir.clone(),
            deadline,
            Vec::new(),
        )
        .await?;
        let delivery = lease.directory.join("tool-deliveries.jsonl");
        let marker = lease.directory.join("worker-pids");
        // No body may run, so the body control is a case-owned port nothing
        // serves: a body that ran anyway is still in the synced ledger.
        let fixture = json!({"scenario":scenario,"delivery_ledger":delivery,
            "provider_ledger":lease.directory.join("provider.jsonl"),
            "body_callback_url":format!("http://127.0.0.1:{}/BodyEntered", base + 12),
            "worker_marker":marker});
        let fixture_path = lease.directory.join("tool-fixture.json");
        super::write(&fixture_path, &fixture)?;
        let environment = std::collections::BTreeMap::from([
            (
                "AGENT_WORKBENCH_TOOL_FIXTURE".to_owned(),
                fixture_path.display().to_string(),
            ),
            ("OPENROUTER_API_KEY".into(), "case-owned-fixture".into()),
            ("AGENT_WORKBENCH_PROTOCOL".into(), "rlm".into()),
            (
                "AGENT_WORKBENCH_RESTATE_ADVERTISE_URL".into(),
                proxy.endpoint.clone(),
            ),
        ]);
        let mut host = WorkbenchHost::new(
            boot.nodes[0].ingress_url.clone(),
            boot.nodes[0].admin_url.clone(),
            base + 10,
            base + 11,
        )?
        .configure(environment)?;
        let ready = host.boot(&artifact, &mut lease).await?;
        let view = RestateView::new(&boot.nodes[0].admin_url, &lease.namespace)?;
        let reader = RestateEvidenceReader::new(
            slug.into(),
            RestateView::new(&boot.nodes[0].admin_url, &lease.namespace)?,
            7,
        );
        let mut evidence = Evidence::empty(slug.into());
        evidence.artifacts = vec![server, artifact];
        evidence
            .stores
            .push(json!({"kind":"workbench_incarnation","ready":ready}));
        Ok(Self {
            slug,
            scenario,
            store_root: lease.directory.join("workbench-data/lash-sessions"),
            barriers: FileBarriers::new(barrier_dir, deadline)?,
            lease,
            cluster,
            host,
            ready,
            proxy,
            reader,
            view,
            delivery,
            marker,
            base,
            evidence,
            seen: BTreeSet::new(),
            killed: Vec::new(),
        })
    }

    fn deadline(&self) -> Instant {
        self.lease.deadline
    }

    /// The barrier a leg's cut of `kind` holds. An X cut names the first
    /// attempt; every other cut names the leg's declared start.
    fn barrier(leg: &Leg, kind: BarrierKind) -> Barrier {
        Barrier {
            work: WorkIdentity {
                ordinal: matches!(kind, BarrierKind::XProposed).then_some(1),
                ..leg.work.clone()
            },
            kind,
        }
    }

    /// Start a leg's Run, bind its invocation while its model answer is
    /// held, arm the leg's cuts, then let the answer return.
    async fn submit(&mut self, leg: &str, cuts: &[BarrierKind]) -> Result<Leg> {
        let alias = format!("{}-{leg}", self.slug);
        let created = self
            .host
            .command(HostCommand::Process {
                action: "create-session".into(),
                input: json!({"session":alias}),
            })
            .await?;
        let session = created.output["session_id"]
            .as_str()
            .context("no actual session")?
            .to_owned();
        let input = format!("{} {leg}", self.scenario);
        let spawned_before = self.pids()?.len();
        let submitted = self
            .host
            .command(HostCommand::Submit {
                session: alias.clone(),
                idempotency_key: alias.clone(),
                input: json!(input),
            })
            .await?;
        self.await_model_request(&input).await?;
        let session_id = lash::SessionId::parse(&session)?;
        let run = lash::TurnId::parse(&submitted.work.run)?;
        let work = loop {
            let stores = lash::sqlite::SqliteStoreSet::open(&self.store_root).await?;
            let store = stores.open_store().await?;
            match self
                .reader
                .bind_public_run(
                    store.as_ref(),
                    &session_id,
                    &run,
                    submitted.work.ingress.clone(),
                )
                .await
            {
                Ok(work) => break work,
                Err(error) => ensure!(
                    Instant::now() < self.deadline(),
                    "{alias}'s Run never bound its invocation: {error:#}"
                ),
            }
            tokio::time::sleep(POLL).await;
        };
        self.proxy
            .bind_invocation(work.segment.clone(), work.clone())?;
        let leg = Leg {
            session,
            work,
            spawned_before,
        };
        for kind in cuts {
            let proposal = Self::barrier(&leg, kind.clone());
            let before_ack = Barrier {
                work: proposal.work.clone(),
                kind: BarrierKind::BeforeAck,
            };
            self.barriers.hold(&proposal)?;
            self.barriers.hold(&before_ack)?;
            self.proxy.arm_cut(TransportCut {
                proposal,
                before_ack,
            })?;
        }
        std::fs::write(
            lash_upgrade_harness::node::tools::release_path(&self.provider_ledger(), &input),
            b"released",
        )?;
        self.evidence
            .stores
            .push(json!({"kind":"leg","leg":alias,"work":leg.work}));
        Ok(leg)
    }

    fn provider_ledger(&self) -> PathBuf {
        self.lease.directory.join("provider.jsonl")
    }

    /// Wait until the scripted model received the leg's input: its answer is
    /// held there until the case releases it.
    async fn await_model_request(&self, input: &str) -> Result<()> {
        loop {
            let text = std::fs::read_to_string(self.provider_ledger()).unwrap_or_default();
            for line in text.lines() {
                let record: Value = serde_json::from_str(line)?;
                if record["input"] == input {
                    return Ok(());
                }
            }
            ensure!(
                Instant::now() < self.deadline(),
                "the model never received {input}"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    async fn hold(&mut self, leg: &Leg, kind: BarrierKind) -> Result<BarrierProof> {
        let barrier = Self::barrier(leg, kind);
        let proof = self.barriers.await_proof(&barrier).await?;
        self.evidence.barriers.push(proof.clone());
        Ok(proof)
    }

    /// Wait for a proposal hold, then let the proposal reach Restate.
    async fn pass(&mut self, leg: &Leg, kind: BarrierKind) -> Result<()> {
        self.hold(leg, kind.clone()).await?;
        self.release(leg, kind)
    }

    fn release(&self, leg: &Leg, kind: BarrierKind) -> Result<()> {
        self.barriers.release(&Self::barrier(leg, kind))
    }

    async fn journal(&self, leg: &Leg) -> Result<Vec<JournalFact>> {
        self.view.journal(&leg.work, &leg.work.segment, 7).await
    }

    async fn events(&self, leg: &Leg) -> Result<Vec<RunEvent>> {
        Ok(self
            .journal(leg)
            .await?
            .into_iter()
            .filter_map(|fact| match fact.decoded {
                Some(DecodedRecord::Run(entry)) => Some(entry.record.events),
                _ => None,
            })
            .flatten()
            .collect())
    }

    async fn starts(&mut self, leg: &Leg) -> Result<Vec<RunEvent>> {
        let journal = self.journal(leg).await?;
        let starts = journal
            .iter()
            .filter_map(|fact| match &fact.decoded {
                Some(DecodedRecord::Run(entry)) => Some(entry.record.events.clone()),
                _ => None,
            })
            .flatten()
            .filter(is_start)
            .collect();
        self.evidence.journals.extend(journal);
        Ok(starts)
    }

    /// Wait for the start event `kind` names in the leg's own journal.
    async fn await_start(&mut self, leg: &Leg, kind: BarrierKind) -> Result<BarrierProof> {
        loop {
            let facts = self.journal(leg).await?;
            let found = facts.iter().find_map(|fact| {
                let Some(DecodedRecord::Run(entry)) = &fact.decoded else {
                    return None;
                };
                entry.record.events.iter().find_map(|event| match (event, &kind) {
                    (RunEvent::StartAdmitted { call_id, .. }, BarrierKind::StartAdmitted)
                    | (RunEvent::StartLaunched { call_id, .. }, BarrierKind::StartRegistered) => {
                        Some((fact.clone(), call_id.to_string()))
                    }
                    _ => None,
                })
            });
            if let Some((fact, call)) = found {
                let mut work = fact.work.clone();
                work.call = Some(call);
                let artifact = self.lease.directory.join(format!(
                    "barrier-{}-{}.json",
                    leg.session,
                    format!("{kind:?}").to_lowercase()
                ));
                super::write(&artifact, &fact)?;
                self.evidence.journals.extend(facts);
                let proof = BarrierProof {
                    barrier: Barrier { work, kind },
                    artifact: artifact.display().to_string(),
                    journal_index: Some(fact.index),
                };
                self.evidence.barriers.push(proof.clone());
                return Ok(proof);
            }
            ensure!(
                Instant::now() < self.deadline(),
                "durable barrier {kind:?} was missed"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// Ask the workbench to cancel the leg's Run and wait until the request
    /// is durable in its store.
    async fn cancel(&mut self, leg: &Leg) -> Result<Value> {
        use lash_core::store::TurnInputStore as _;
        let requested = self
            .host
            .command(HostCommand::Cancel {
                run: leg.work.run.clone(),
            })
            .await?;
        let address = lash::TurnAddress::new(
            lash::SessionId::parse(&leg.session)?,
            lash::TurnId::parse(&leg.work.run)?,
        );
        loop {
            let stores = lash::sqlite::SqliteStoreSet::open(&self.store_root).await?;
            let store = stores.open_store().await?;
            if let Some(record) = store.turn_cancel_request(&address).await? {
                return Ok(json!({"requested":requested.output,"record":record}));
            }
            ensure!(
                Instant::now() < self.deadline(),
                "the cancel request never became durable"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// The leg's settled report, once its Run has a store terminal.
    async fn settled(&mut self, leg: &Leg) -> Result<RemoteTurnReport> {
        use lash_core::store::RunStore as _;
        let session = lash::SessionId::parse(&leg.session)?;
        let run = lash::TurnId::parse(&leg.work.run)?;
        loop {
            let stores = lash::sqlite::SqliteStoreSet::open(&self.store_root).await?;
            if stores
                .open_store()
                .await?
                .run_terminal(&session, &run)
                .await?
                .is_some()
            {
                break;
            }
            ensure!(
                Instant::now() < self.deadline(),
                "{}'s Run never settled",
                leg.work.run
            );
            tokio::time::sleep(POLL).await;
        }
        let outcome = self
            .host
            .command(HostCommand::Attach {
                run: leg.work.run.clone(),
            })
            .await?;
        let remote: lash_remote_protocol::RemoteSendOutcome = serde_json::from_value(
            outcome
                .output
                .get("outcome")
                .cloned()
                .context("Attach has no typed outcome")?,
        )?;
        let lash_remote_protocol::RemoteSendOutcome::Settled { report, .. } = remote else {
            bail!("the leg's input did not settle: {:?}", outcome.output);
        };
        report.validate()?;
        ensure!(
            report.turn_id.as_str() == leg.work.run,
            "the follow answered another Run"
        );
        self.evidence.outputs.push(outcome);
        Ok(*report)
    }

    /// SIGKILL the serving workbench process alone: a crash of the host, so
    /// a worker it spawned outlives it unless the product reaps it.
    async fn kill(&mut self, proof: BarrierProof) -> Result<()> {
        let pid = self.ready.process.pid;
        let killed = self
            .host
            .command(HostCommand::Process {
                action: "kill-host-process".into(),
                input: json!({}),
            })
            .await?;
        ensure!(
            killed.output["killed"] == true
                && killed.output["reaped"] == true
                && killed.output["process"]["pid"] == pid,
            "kill did not reap the serving workbench"
        );
        ensure!(gone(pid), "the killed workbench {pid} was not reaped");
        self.killed.push(pid);
        self.evidence.faults.push(FaultReceipt {
            fault: Fault::KillHost {
                target: "workbench".into(),
            },
            proof,
            target_incarnation: self.ready.process.incarnation,
        });
        Ok(())
    }

    /// Serve again under the registered deployment, without re-registering.
    async fn restart(&mut self) -> Result<()> {
        let ready = self
            .host
            .command(HostCommand::Process {
                action: "restart-in-place".into(),
                input: json!({}),
            })
            .await?;
        self.ready = serde_json::from_value(ready.output)?;
        self.evidence
            .stores
            .push(json!({"kind":"workbench_incarnation","ready":self.ready}));
        Ok(())
    }

    async fn rows(&self) -> Result<Vec<Row>> {
        let path = self
            .store_root
            .join(lash_sqlite_store::SqliteDatabase::ProcessRegistry.file_name());
        tokio::task::spawn_blocking(move || -> Result<Vec<Row>> {
            if !path.exists() {
                return Ok(Vec::new());
            }
            let db = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let mut statement = db.prepare(
                "SELECT process_id, start_key, consumer_hold_key, cancel_requested_at_ms
                 FROM processes WHERE start_key LIKE ?1 ORDER BY process_id",
            )?;
            let rows = statement
                .query_map([format!("{ISOLATED_KEY}%")], |row| {
                    Ok(Row {
                        process_id: row.get(0)?,
                        start_key: row.get(1)?,
                        hold: row.get(2)?,
                        cancel_requested: row.get::<_, Option<i64>>(3)?.is_some(),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?
    }

    /// The isolated rows no earlier leg owned.
    async fn rows_for(&self, _leg: &Leg) -> Result<Vec<Row>> {
        Ok(self
            .rows()
            .await?
            .into_iter()
            .filter(|row| !self.seen.contains(&row.start_key))
            .collect())
    }

    /// Every worker PID the engine's workers wrote, in spawn order.
    fn pids(&self) -> Result<Vec<u32>> {
        match std::fs::read_to_string(&self.marker) {
            Ok(text) => text.lines().map(|line| Ok(line.trim().parse()?)).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn spawned(&self, leg: &Leg) -> Result<Vec<u32>> {
        Ok(self
            .pids()?
            .split_off(leg.spawned_before.min(self.pids()?.len())))
    }

    async fn await_spawn(&self, leg: &Leg) -> Result<u32> {
        loop {
            if let Some(pid) = self.spawned(leg)?.first() {
                return Ok(*pid);
            }
            ensure!(
                Instant::now() < self.deadline(),
                "the launched start spawned no worker"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    fn deliveries(&self) -> Result<usize> {
        match std::fs::read_to_string(&self.delivery) {
            Ok(text) => Ok(text.lines().filter(|line| !line.trim().is_empty()).count()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error.into()),
        }
    }

    async fn assert_never_started(&mut self, leg: &Leg) -> Result<()> {
        ensure!(self.deliveries()? == 0, "an ordinary body ran");
        ensure!(self.spawned(leg)?.is_empty(), "a worker was spawned");
        ensure!(
            self.rows_for(leg).await?.is_empty(),
            "a process identity was admitted"
        );
        let starts = self.starts(leg).await?;
        ensure!(starts.is_empty(), "a start was recorded: {starts:?}");
        Ok(())
    }

    async fn assert_recovered(
        &mut self,
        leg: &Leg,
        descriptor: &IsolatedProcessDescriptor,
        before: Option<(&str, u32)>,
    ) -> Result<()> {
        ensure!(
            descriptor.start_key.as_str().starts_with(ISOLATED_KEY)
                && descriptor.boundary == ProcessExecutionBoundary::WorkerProcess
                && descriptor.termination.is_none(),
            "descriptor {descriptor:?}"
        );
        let starts = self.starts(leg).await?;
        let start_key = &descriptor.start_key;
        ensure!(
            matches!(
                starts.as_slice(),
                [
                    RunEvent::StartAdmitted { start_key: admitted, .. },
                    RunEvent::StartLaunched { start_key: launched, process_id, .. },
                    RunEvent::StartDischarged { start_key: discharged, cancelled: false, .. },
                ] if *process_id == descriptor.process_id
                    && admitted == start_key && launched == start_key && discharged == start_key
            ),
            "start events {starts:?}"
        );
        let rows = self.rows_for(leg).await?;
        ensure!(
            rows.len() == 1
                && rows[0].process_id == descriptor.process_id.to_string()
                && rows[0].start_key == start_key.to_string()
                && rows[0].hold.is_none(),
            "registry {rows:?}"
        );
        let pid = self.await_spawn(leg).await?;
        if let Some((process, registered)) = before {
            ensure!(
                descriptor.process_id.to_string() == process,
                "recovery admitted another process identity"
            );
            // The redelivered process workflow is the one recovery could
            // spawn a replacement from: once Restate retried it on this
            // incarnation, the recovered process has exactly the worker it
            // runs on.
            self.await_redelivered(&descriptor.process_id.to_string())
                .await?;
            let spawned = self.spawned(leg)?;
            ensure!(
                spawned == vec![registered],
                "recovery replaced the registered worker {registered}: spawned {spawned:?}, \
                 alive {:?}",
                spawned
                    .iter()
                    .map(|pid| (*pid, alive(*pid)))
                    .collect::<Vec<_>>()
            );
        } else {
            ensure!(
                self.spawned(leg)? == vec![pid] && alive(pid),
                "the admitted worker is not the one running one"
            );
            ensure!(
                parent(pid) == Some(self.ready.process.pid),
                "the worker is not a child of the serving workbench"
            );
        }
        ensure!(self.deliveries()? == 0, "an ordinary body ran");
        self.evidence
            .stores
            .push(json!({"kind":"isolated_recovered","leg":leg.work,
            "descriptor":descriptor,"rows":rows,"spawned":self.spawned(leg)?}));
        self.seen.insert(start_key.to_string());
        Ok(())
    }

    /// Wait until Restate re-attempted the process's workflow run after the
    /// kill, on the serving incarnation.
    async fn await_redelivered(&mut self, process: &str) -> Result<()> {
        loop {
            let segments = self.view.process_segments(process).await?;
            if segments
                .iter()
                .any(|segment| segment.invocation.retry_count.unwrap_or(0) > 0)
            {
                let artifact = self
                    .lease
                    .directory
                    .join(format!("process-segments-{process}.json"));
                super::write(
                    &artifact,
                    &json!({"process":process,"segments":format!("{segments:?}")}),
                )?;
                self.evidence
                    .stores
                    .push(json!({"kind":"process_redelivered","artifact":artifact}));
                return Ok(());
            }
            ensure!(
                Instant::now() < self.deadline(),
                "the process workflow was never redelivered: {segments:?}"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    async fn finish(mut self, id: &str, result: Result<()>) -> Result<()> {
        let mut errors = Vec::new();
        if let Err(error) = &result {
            errors.push(format!("{error:#}"));
        }
        let pids = self.pids().unwrap_or_default();
        match self.rows().await {
            Ok(rows) => self.evidence.stores.push(
                json!({"kind":"isolated_final_state","pids":pids.iter().map(|pid| json!({"pid":pid,"alive":alive(*pid)})).collect::<Vec<_>>(),"rows":rows}),
            ),
            Err(error) => errors.push(format!("final state read: {error:#}")),
        }
        match self.host.trace_records() {
            Ok(records) => self.evidence.effects.extend(records),
            Err(error) => errors.push(format!("trace read: {error:#}")),
        }
        // A worker its killed workbench left running is owned by nobody the
        // product can reach; the case kills it so the run does not leak, and
        // the leg's oracle has already judged it.
        for pid in pids.iter().copied().collect::<BTreeSet<_>>() {
            if alive(pid) {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
        for group in std::mem::take(&mut self.killed) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{group}")])
                .status();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !members(group).is_empty() && Instant::now() < deadline {
                tokio::time::sleep(POLL).await;
            }
            let left = members(group);
            self.evidence.cleanup.push(CleanupReceipt {
                resource: format!("process-group:{group}"),
                closed: left.is_empty(),
                detail: format!("killed workbench's group; running members {left:?}"),
            });
        }
        match self.host.stop().await {
            Ok(receipts) => self.evidence.cleanup.extend(receipts),
            Err(error) => {
                errors.push(format!("workbench cleanup: {error:#}"));
                self.evidence.cleanup.push(CleanupReceipt {
                    resource: "workbench".into(),
                    closed: false,
                    detail: format!("{error:#}"),
                });
            }
        }
        match self.proxy.finish().await {
            Ok(()) => self.evidence.cleanup.push(CleanupReceipt {
                resource: format!("listener:{}", self.base + 13),
                closed: true,
                detail: "proxy tasks joined and listener refused connection".into(),
            }),
            Err(error) => errors.push(format!("proxy cleanup: {error:#}")),
        }
        // A worker whose workbench was killed is adopted by the test
        // runner's subreaper, which reaps it when the test exits.
        for pid in pids.into_iter().collect::<BTreeSet<_>>() {
            let deadline = Instant::now() + Duration::from_secs(30);
            while alive(pid) && Instant::now() < deadline {
                tokio::time::sleep(POLL).await;
            }
            self.evidence.cleanup.push(CleanupReceipt {
                resource: format!("worker:{pid}"),
                closed: !alive(pid),
                detail: match stat(pid) {
                    None => "reaped: absent from /proc".to_owned(),
                    Some((state, parent)) => format!(
                        "terminated (state {state}); parent {parent} reaps it when the test exits"
                    ),
                },
            });
        }
        match self.cluster.finish().await {
            Ok(receipts) => self.evidence.cleanup.extend(receipts),
            Err(error) => {
                errors.push(format!("cluster cleanup: {error:#}"));
                self.evidence.cleanup.push(CleanupReceipt {
                    resource: "cluster".into(),
                    closed: false,
                    detail: format!("{error:#}"),
                });
            }
        }
        if self.evidence.cleanup.iter().any(|receipt| !receipt.closed) {
            errors.push("case leaked owned resources".into());
        }
        let directory = self.lease.directory.clone();
        super::write(&directory.join("evidence.json"), &self.evidence)?;
        let error = (!errors.is_empty()).then(|| errors.join("; "));
        let verdict = error
            .as_ref()
            .map_or(Verdict::Passed, |reason| Verdict::Failed {
                reason: reason.clone(),
            });
        let counts = CaseReceipt {
            evidence: self.evidence,
            verdict,
        }
        .write(&directory)?;
        super::write(
            &directory.join("result.json"),
            &json!({"scenario":id,"variant":"default","channel":"rlm","selected":counts.selected,
                "executed":counts.executed,"passed":counts.passed,"failed":counts.failed,
                "not_run":counts.not_run,"error":error}),
        )?;
        if let Some(error) = error {
            bail!("{error}");
        }
        counts.reconcile()?;
        println!("{id} workbench rlm selected=1 executed=1 passed=1 failed=0 not_run=0");
        Ok(())
    }
}

/// The state and parent fields of `/proc/<pid>/stat`, when the pid exists.
fn stat(pid: u32) -> Option<(char, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?.chars().next()?;
    Some((state, fields.next()?.parse().ok()?))
}

/// The running members of process group `group`.
fn members(group: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
                    let state = fields.next()?;
                    let _parent = fields.next()?;
                    let pgrp: u32 = fields.next()?.parse().ok()?;
                    Some(pgrp == group && state != "Z" && state != "X")
                })
                .unwrap_or(false)
        })
        .collect()
}

fn parent(pid: u32) -> Option<u32> {
    stat(pid).map(|(_, parent)| parent)
}

/// Running: present and not a zombie awaiting its reaper.
fn alive(pid: u32) -> bool {
    stat(pid).is_some_and(|(state, _)| state != 'Z' && state != 'X')
}

/// Reaped: the pid no longer exists, not even as a zombie.
fn gone(pid: u32) -> bool {
    stat(pid).is_none()
}
