//! S19/S20 (L08) on the served upgrade node: an isolated call admitted under
//! a physical worker engine, crash cuts around launch and discharge, and the
//! worker's termination observed at the OS level.
//!
//! The node is a real process over file SQLite and the case's Restate server;
//! the worker is a real OS process that writes its own PID marker. Cuts are
//! armed by node gates and proven by the decoded Run journal of the actual
//! invocation. No sleep establishes readiness or completion.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use lash_core::tool_dispatch::{
    IsolatedProcessDescriptor, ProcessExecutionBoundary, WorkerTerminationReceipt,
};
use lash_core::tool_run::RunEvent;
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseLease};
use lash_upgrade_harness::e2e::cluster::{ClusterControl, LocalCluster};
use lash_upgrade_harness::e2e::control::{
    Barrier, BarrierKind, BarrierProof, CleanupReceipt, Fault, FaultReceipt, WorkIdentity,
};
use lash_upgrade_harness::e2e::evidence::{CaseReceipt, DecodedRecord, Evidence, Verdict};
use lash_upgrade_harness::harness::{Case, NodeBinary, ServeOptions, Services, ServingNode};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::h3::{self, H3Command, IsolatedArgs, IsolatedClaim, WorkerMarker};
use serde_json::{Value, json};

const CASE_DEADLINE: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_millis(25);
const BOUNDARY_REFUSAL: &str =
    "process implementation `e2e-h3-worker` supplies Invocation, not WorkerProcess";

/// L08: unsupported isolation refuses before launch; host SIGKILL between
/// admission and launch, and between registration and send, recovers the same
/// StartKey and one admitted process identity; no ordinary body ever runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the upgrade node binary and a private Restate server supplied by the E2E gate"]
async fn s19_isolated_start_cuts_recover_one_worker() -> Result<()> {
    let mut host = Host::boot("s19-isolated").await?;
    let result = s19(&mut host).await;
    host.finish("S19", result).await
}

/// L08: cancellation before admission forbids the start; cancellation after
/// start terminates and reaps the worker, and a coordinator killed after the
/// worker's death but before its launch and discharge are durable recovers the
/// retained receipt and releases the hold only after termination.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the upgrade node binary and a private Restate server supplied by the E2E gate"]
async fn s20_isolated_cancel_terminates_and_reaps_worker() -> Result<()> {
    let mut host = Host::boot("s20-isolated").await?;
    let result = s20(&mut host).await;
    host.finish("S20", result).await
}

async fn s19(host: &mut Host) -> Result<()> {
    let key = "s19-unsupported";
    let run = host.submit(host.args(key, IsolatedClaim::UnsupportedBoundary))?;
    let terminal = host.await_terminal(key).await?;
    let error = terminal["error"]
        .as_str()
        .ok_or_else(|| anyhow!("unsupported boundary was not refused: {terminal}"))?;
    ensure!(
        error.contains(BOUNDARY_REFUSAL),
        "unsupported boundary refused with another cause: {error}"
    );
    let followed = host.follow(key, &run);
    ensure!(
        followed.is_err(),
        "refused isolation answered: {followed:?}"
    );
    host.assert_never_started(key, &run).await?;
    host.evidence.effects.push(json!({"kind":"s19_unsupported_boundary","run":run,
        "terminal":terminal,"follow_error":format!("{:#}", followed.err().unwrap_or_else(|| anyhow!("none")))}));

    let key = "s19-admission";
    let mut args = host.args(key, IsolatedClaim::Physical);
    args.hold_launch = Some("s19-launch".into());
    let run = host.submit(args)?;
    host.await_gate("s19-launch")?;
    let proof = host
        .await_start(key, &run, BarrierKind::StartAdmitted)
        .await?;
    ensure!(
        !host.events(key, &run).await?.iter().any(is_launched),
        "launch was durable before the admission cut"
    );
    ensure!(
        host.spawns()?.is_empty() && host.rows_for(key)?.is_empty(),
        "a worker or process row exists before launch"
    );
    host.kill(proof)?;
    host.case.release("s19-launch")?;
    host.restart()?;
    let descriptor = host.descriptor(key, &run).await?;
    host.assert_recovered(key, &run, &descriptor, None).await?;

    let key = "s19-registered";
    let mut args = host.args(key, IsolatedClaim::Physical);
    args.hold_registered = Some("s19-registered".into());
    let run = host.submit(args)?;
    host.await_gate("s19-registered")?;
    let rows = host.rows_for(key)?;
    ensure!(rows.len() == 1, "registration left {} rows", rows.len());
    let process = lash_core::ProcessId::parse(&rows[0].process_id)?;
    let marker = host
        .marker(&process)?
        .context("registered worker wrote no marker")?;
    ensure!(alive(marker.pid), "registered worker is not running");
    let proof = host
        .await_start(key, &run, BarrierKind::StartAdmitted)
        .await?;
    ensure!(
        !host.events(key, &run).await?.iter().any(is_launched),
        "send was durable before the registration cut"
    );
    host.kill(proof)?;
    ensure!(
        alive(marker.pid),
        "the worker did not outlive its killed coordinator"
    );
    host.case.release("s19-registered")?;
    host.restart()?;
    let descriptor = host.descriptor(key, &run).await?;
    host.assert_recovered(key, &run, &descriptor, Some((&process, marker.pid)))
        .await
}

async fn s20(host: &mut Host) -> Result<()> {
    let key = "s20-pre-admission";
    let mut args = host.args(key, IsolatedClaim::Physical);
    args.hold_prepare = Some("s20-prepare".into());
    let run = host.submit(args)?;
    host.await_gate("s20-prepare")?;
    let cancel = host.cancel(key, &run)?;
    ensure!(
        cancel["cancel_requested"] == true,
        "cancel was not requested: {cancel}"
    );
    host.case.release("s20-prepare")?;
    let terminal = host.await_run_terminal(key, &run).await?;
    host.assert_never_started(key, &run).await?;
    host.evidence
        .effects
        .push(json!({"kind":"s20_pre_admission_cancel","run":run,
        "run_terminal":terminal,"terminal":host.terminal(key)?}));

    let key = "s20-cancel";
    let mut args = host.args(key, IsolatedClaim::Physical);
    args.hold_registered = Some("s20-launched".into());
    args.hold_discharge = Some("s20-discharge".into());
    let run = host.submit(args)?;
    host.await_gate("s20-launched")?;
    let rows = host.rows_for(key)?;
    ensure!(
        rows.len() == 1 && rows[0].hold.is_some(),
        "launched start is not held: {rows:?}"
    );
    let process = lash_core::ProcessId::parse(&rows[0].process_id)?;
    let marker = host
        .marker(&process)?
        .context("launched worker wrote no marker")?;
    ensure!(alive(marker.pid), "launched worker is not running");
    let cancel = host.cancel(key, &run)?;
    ensure!(
        cancel["cancel_requested"] == true,
        "cancel was not requested: {cancel}"
    );
    let held = host.rows_for(key)?;
    ensure!(
        alive(marker.pid),
        "a cancel request alone terminated the worker"
    );
    ensure!(
        held.len() == 1 && held[0].hold.is_some(),
        "a cancel request alone released the hold: {held:?}"
    );
    host.evidence
        .stores
        .push(json!({"kind":"s20_after_cancel_request","rows":held,
        "worker_pid":marker.pid,"alive":true}));
    host.case.release("s20-launched")?;
    host.await_gate("s20-discharge")?;
    let receipt: WorkerTerminationReceipt = read_json(&h3::receipt_file(&host.workers, &process))?
        .context("the worker was not terminated before discharge")?;
    ensure!(
        receipt.process_id == process && receipt.worker_pid.get() == marker.pid,
        "termination receipt names another worker: {receipt:?}"
    );
    ensure!(
        gone(marker.pid),
        "terminated worker {} was not reaped",
        marker.pid
    );
    let held = host.rows_for(key)?;
    ensure!(
        held.len() == 1 && held[0].hold.is_some(),
        "hold was released before the discharge: {held:?}"
    );
    // Launch and discharge are one durable preparation (FIG-5009): while
    // discharge is held, only the admission is in the Run's journal.
    let proof = host
        .await_start(key, &run, BarrierKind::StartAdmitted)
        .await?;
    ensure!(
        !host.events(key, &run).await?.iter().any(|event| matches!(
            event,
            RunEvent::StartLaunched { .. } | RunEvent::StartDischarged { .. }
        )),
        "launch or discharge was ACKed before the coordinator cut"
    );
    host.evidence
        .stores
        .push(json!({"kind":"s20_before_discharge_ack","rows":held,
        "receipt":receipt,"worker_pid":marker.pid,"reaped":true}));
    host.kill(proof)?;
    host.case.release("s20-discharge")?;
    host.restart()?;
    let discharged = host
        .await_start(key, &run, BarrierKind::ConsumerHoldDischarged)
        .await?;
    host.evidence.barriers.push(discharged);
    let events = host.events(key, &run).await?;
    let starts: Vec<_> = events.iter().filter(|event| is_start(event)).collect();
    ensure!(
        matches!(
            starts.as_slice(),
            [
                RunEvent::StartAdmitted { .. },
                RunEvent::StartLaunched { process_id, .. },
                RunEvent::StartDischarged { cancelled: true, .. },
            ] if *process_id == process
        ),
        "cancelled start did not record admit/launch/discharge(cancelled) once: {starts:?}"
    );
    let rows = host.rows_for(key)?;
    ensure!(
        rows == vec![Row::drained(
            &process,
            key,
            host.environment(&rows).await?,
            true
        )],
        "discharge left the registry at {rows:?}"
    );
    ensure!(
        host.spawns_for(&process)?.len() == 1,
        "recovery spawned a replacement worker"
    );
    let recovered: WorkerTerminationReceipt =
        read_json(&h3::receipt_file(&host.workers, &process))?
            .context("retained receipt vanished")?;
    ensure!(
        recovered == receipt,
        "recovery terminated a different worker"
    );
    let descriptor = host.descriptor(key, &run).await?;
    ensure!(
        descriptor.process_id == process
            && descriptor.start_key == h3::isolated_start_key(key)
            && descriptor.boundary == ProcessExecutionBoundary::WorkerProcess
            && descriptor.termination.as_ref() == Some(&receipt),
        "descriptor lost the recorded termination: {descriptor:?}"
    );
    ensure!(
        !h3::body_marker(&host.workers, key).exists(),
        "an ordinary body ran"
    );
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

/// One process row as read back from the case's registry file.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct Row {
    process_id: String,
    start_key: Option<String>,
    environment: Option<String>,
    hold: Option<String>,
    cancel_requested: bool,
}

impl Row {
    fn drained(
        process: &lash_core::ProcessId,
        key: &str,
        environment: String,
        cancel_requested: bool,
    ) -> Self {
        Self {
            process_id: process.to_string(),
            start_key: Some(h3::isolated_start_key(key).to_string()),
            environment: Some(environment),
            hold: None,
            cancel_requested,
        }
    }
}

struct Host {
    lease: CaseLease,
    cluster: LocalCluster,
    case: Case,
    node: NodeBinary,
    serving: Option<ServingNode>,
    bind: String,
    incarnation: u32,
    workers: PathBuf,
    evidence: Evidence,
}

impl Host {
    async fn boot(slug: &str) -> Result<Self> {
        let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
        std::fs::create_dir_all(&root)?;
        let deadline = Instant::now() + CASE_DEADLINE;
        let mut lease = CaseLease::new(slug, root.join(slug), deadline)?;
        let base: u16 = std::env::var("LASH_E2E_PORT_BASE")?.parse()?;
        let server = artifact(
            "restate-server",
            std::env::var("LASH_RESTATE_SERVER_BIN")?.into(),
        )?;
        let node_artifact = artifact("upgrade-node", std::env::var("LASH_UPGRADE_NODE_N")?.into())?;
        server.verify()?;
        node_artifact.verify()?;
        let mut cluster = LocalCluster::new(base, deadline);
        let boot = cluster.boot(&server, 1, &mut lease).await?;
        let services = Services {
            ingress_url: boot.nodes[0].ingress_url.clone(),
            admin_url: boot.nodes[0].admin_url.clone(),
            postgres_url: String::new(),
        };
        let case = Case::leased_sqlite(slug, &services, &lease)?;
        let node = NodeBinary::at(node_artifact.path.clone(), BuildLabel::N);
        let serving = tokio::task::block_in_place(|| node.serve(&case))?;
        let bind = serving.bind()?;
        let workers = lease.directory.join("workers");
        std::fs::create_dir_all(&workers)?;
        let mut evidence = Evidence::empty(slug.into());
        evidence.artifacts = vec![server, node_artifact];
        evidence.stores.push(
            json!({"kind":"node_incarnation","incarnation":1,"pid":serving.pid()?,"bind":bind}),
        );
        Ok(Self {
            lease,
            cluster,
            case,
            node,
            serving: Some(serving),
            bind,
            incarnation: 1,
            workers,
            evidence,
        })
    }

    fn args(&self, key: &str, claim: IsolatedClaim) -> IsolatedArgs {
        IsolatedArgs {
            key: key.to_owned(),
            worker_dir: self.workers.clone(),
            claim,
            hold_prepare: None,
            hold_launch: None,
            hold_registered: None,
            hold_discharge: None,
        }
    }

    fn h3(&self, session: &str, command: &H3Command) -> Result<Value> {
        tokio::task::block_in_place(|| self.node.h3(&self.case, session, command))
    }

    fn submit(&self, args: IsolatedArgs) -> Result<lash::TurnId> {
        let key = args.key.clone();
        let admitted = self.h3(
            &key,
            &H3Command::Isolated {
                key: key.clone(),
                args,
            },
        )?;
        ensure!(admitted["admitted"] == true, "{key} was not admitted");
        Ok(serde_json::from_value(admitted["run"].clone())?)
    }

    fn follow(&self, key: &str, run: &lash::TurnId) -> Result<Value> {
        self.h3(key, &H3Command::Follow { run: run.clone() })
    }

    fn cancel(&self, key: &str, run: &lash::TurnId) -> Result<Value> {
        self.h3(key, &H3Command::Cancel { run: run.clone() })
    }

    fn await_gate(&self, gate: &str) -> Result<()> {
        tokio::task::block_in_place(|| self.case.await_gate(gate)).map(drop)
    }

    fn deadline(&self) -> Instant {
        self.lease.deadline
    }

    fn terminal(&self, key: &str) -> Result<Option<Value>> {
        read_json(&h3::terminal_file(&self.workers, key))
    }

    async fn await_terminal(&self, key: &str) -> Result<Value> {
        loop {
            if let Some(terminal) = self.terminal(key)? {
                return Ok(terminal);
            }
            ensure!(
                Instant::now() < self.deadline(),
                "{key} never wrote its terminal"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    async fn stores(&self) -> Result<lash::sqlite::SqliteStoreSet> {
        let dir = self.case.sqlite_dir().context("case is not SQLite")?;
        Ok(lash::sqlite::SqliteStoreSet::open(dir).await?)
    }

    /// The store terminal of a Run the test does not expect to answer.
    async fn await_run_terminal(&self, key: &str, run: &lash::TurnId) -> Result<Value> {
        let session = lash::SessionId::fixture(key.to_owned());
        loop {
            let stores = self.stores().await?;
            if let Some(terminal) = lash::StoreSet::session_store_factory(&stores)
                .run_terminal(&session, run)
                .await?
            {
                return Ok(json!(format!("{:?}", terminal.kind())));
            }
            ensure!(
                Instant::now() < self.deadline(),
                "{key}'s Run never settled"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    async fn invocation(&self, key: &str, run: &lash::TurnId) -> Result<String> {
        use lash_core::store::RunStore as _;
        #[derive(serde::Deserialize)]
        struct Invocation {
            id: String,
        }
        let session = lash::SessionId::fixture(key.to_owned());
        let view = self.case.view()?;
        let prefix = view.service_name("LashTurn").replace('\'', "''");
        loop {
            let stores = self.stores().await?;
            let store = stores.open_store().await?;
            if store.run_executor(&session, run).await?.is_some()
                && let Some(executor) =
                    lash_restate::recorded_turn_invocation_key(store.as_ref(), &session, run)
                        .await?
            {
                let rows: Vec<Invocation> = view
                    .query(&format!(
                        "SELECT id FROM sys_invocation WHERE target_service_name LIKE '{prefix}%' \
                         AND target_service_key = '{}' AND target_handler_name = 'run'",
                        executor.replace('\'', "''")
                    ))
                    .await?;
                match rows.as_slice() {
                    [one] => return Ok(one.id.clone()),
                    [] => {}
                    many => bail!("{key}'s Run has {} invocations", many.len()),
                }
            }
            ensure!(
                Instant::now() < self.deadline(),
                "{key}'s Run never reached its invocation"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    async fn journal(
        &self,
        key: &str,
        run: &lash::TurnId,
    ) -> Result<Vec<lash_upgrade_harness::e2e::evidence::JournalFact>> {
        let invocation = self.invocation(key, run).await?;
        let work = WorkIdentity {
            ingress: key.to_owned(),
            run: run.to_string(),
            segment: invocation.clone(),
            call: None,
            ordinal: None,
        };
        self.case.view()?.journal(&work, &invocation, 7).await
    }

    async fn events(&self, key: &str, run: &lash::TurnId) -> Result<Vec<RunEvent>> {
        Ok(self
            .journal(key, run)
            .await?
            .into_iter()
            .filter_map(|fact| match fact.decoded {
                Some(DecodedRecord::Run(entry)) => Some(entry.record.events),
                _ => None,
            })
            .flatten()
            .collect())
    }

    /// Wait for the start event `kind` names in the Run's own journal.
    async fn await_start(
        &mut self,
        key: &str,
        run: &lash::TurnId,
        kind: BarrierKind,
    ) -> Result<BarrierProof> {
        loop {
            let facts = self.journal(key, run).await?;
            let found = facts.iter().find_map(|fact| {
                let Some(DecodedRecord::Run(entry)) = &fact.decoded else {
                    return None;
                };
                entry
                    .record
                    .events
                    .iter()
                    .find_map(|event| match (event, &kind) {
                        (RunEvent::StartAdmitted { call_id, .. }, BarrierKind::StartAdmitted)
                        | (
                            RunEvent::StartDischarged { call_id, .. },
                            BarrierKind::ConsumerHoldDischarged,
                        ) => Some((fact.clone(), call_id.to_string())),
                        _ => None,
                    })
            });
            if let Some((fact, call)) = found {
                let mut work = fact.work.clone();
                work.call = Some(call);
                let artifact = self.lease.directory.join(format!(
                    "barrier-{key}-{}.json",
                    format!("{kind:?}").to_lowercase()
                ));
                write(&artifact, &fact)?;
                self.evidence.journals.extend(facts);
                let proof = BarrierProof {
                    barrier: Barrier { work, kind },
                    artifact: artifact.display().to_string(),
                    journal_index: Some(fact.index),
                };
                self.evidence.barriers.push(proof.clone());
                return Ok(proof);
            }
            if let Some(serving) = self.serving.as_mut() {
                serving.assert_running()?;
            }
            ensure!(
                Instant::now() < self.deadline(),
                "{key}: durable barrier {kind:?} was missed"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    fn kill(&mut self, proof: BarrierProof) -> Result<()> {
        let serving = self.serving.as_mut().context("no serving node")?;
        let pid = serving.pid()?;
        serving.kill_and_reap()?;
        ensure!(gone(pid), "killed node {pid} was not reaped");
        self.evidence.faults.push(FaultReceipt {
            fault: Fault::KillHost {
                target: "upgrade-node".into(),
            },
            proof,
            target_incarnation: self.incarnation,
        });
        self.serving = None;
        Ok(())
    }

    fn restart(&mut self) -> Result<()> {
        ensure!(self.serving.is_none(), "the previous node still serves");
        let options = ServeOptions {
            bind: Some(self.bind.clone()),
            unregistered: true,
            register_later: false,
        };
        let serving = tokio::task::block_in_place(|| self.node.serve_with(&self.case, &options))?;
        self.incarnation += 1;
        self.evidence.stores.push(json!({"kind":"node_incarnation",
            "incarnation":self.incarnation,"pid":serving.pid()?,"bind":self.bind}));
        self.serving = Some(serving);
        Ok(())
    }

    async fn descriptor(&self, key: &str, run: &lash::TurnId) -> Result<IsolatedProcessDescriptor> {
        let terminal = self.await_run_terminal(key, run).await?;
        let presentation = match self.follow(key, run) {
            Ok(followed) => followed["output"]
                .as_str()
                .map(str::to_owned)
                .context("follow answered no output")?,
            Err(error) => {
                let retained = self.await_terminal(key).await?;
                retained["presentation"]
                    .as_str()
                    .map(str::to_owned)
                    .with_context(|| {
                        format!("{key} has no descriptor (run {terminal}; follow {error:#})")
                    })?
            }
        };
        Ok(serde_json::from_str(&presentation)?)
    }

    fn rows(&self) -> Result<Vec<Row>> {
        let dir = self.case.sqlite_dir().context("case is not SQLite")?;
        let path = dir.join(lash_sqlite_store::SqliteDatabase::ProcessRegistry.file_name());
        let db = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut statement = db.prepare(
            "SELECT process_id, start_key, record_json, consumer_hold_key, cancel_requested_at_ms
             FROM processes ORDER BY process_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(process_id, start_key, record, hold, cancel)| {
                let record: Value = serde_json::from_str(&record)?;
                Ok(Row {
                    process_id,
                    start_key,
                    environment: record["env_ref"].as_str().map(str::to_owned),
                    hold,
                    cancel_requested: cancel.is_some(),
                })
            })
            .collect()
    }

    async fn environment(&self, rows: &[Row]) -> Result<String> {
        let [row] = rows else {
            bail!("expected one isolated process: {rows:?}");
        };
        let reference = row
            .environment
            .as_ref()
            .context("process has no environment")?;
        let stores = self.stores().await?;
        let spec = lash_core::runtime::load_process_execution_env(
            lash::StoreSet::process_env_store(&stores).as_ref(),
            &lash_core::ProcessExecutionEnvRef::new(reference),
        )
        .await?;
        ensure!(
            spec.stable_ref()?.as_str() == reference,
            "environment is not content-addressed"
        );
        Ok(reference.clone())
    }

    fn rows_for(&self, key: &str) -> Result<Vec<Row>> {
        let start = h3::isolated_start_key(key).to_string();
        Ok(self
            .rows()?
            .into_iter()
            .filter(|row| row.start_key.as_deref() == Some(start.as_str()))
            .collect())
    }

    fn spawns(&self) -> Result<Vec<h3::WorkerSpawn>> {
        h3::spawns(&self.workers)
    }

    fn spawns_for(&self, process: &lash_core::ProcessId) -> Result<Vec<h3::WorkerSpawn>> {
        Ok(self
            .spawns()?
            .into_iter()
            .filter(|spawn| spawn.process_id == *process)
            .collect())
    }

    fn marker(&self, process: &lash_core::ProcessId) -> Result<Option<WorkerMarker>> {
        read_json(&h3::worker_file(&self.workers, process))
    }

    fn markers(&self) -> Result<Vec<WorkerMarker>> {
        let mut markers = Vec::new();
        for entry in std::fs::read_dir(&self.workers)? {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("worker-") && name.ends_with(".json") {
                markers.extend(read_json::<WorkerMarker>(&path)?);
            }
        }
        Ok(markers)
    }

    async fn assert_never_started(&mut self, key: &str, run: &lash::TurnId) -> Result<()> {
        ensure!(
            !h3::body_marker(&self.workers, key).exists(),
            "{key}: an ordinary body ran"
        );
        // Each never-started call opens its case, so no worker exists yet.
        ensure!(self.spawns()?.is_empty(), "{key}: a worker was spawned");
        ensure!(
            self.rows_for(key)?.is_empty(),
            "{key}: a process identity was admitted"
        );
        let journal = self.journal(key, run).await?;
        let starts: Vec<RunEvent> = journal
            .iter()
            .filter_map(|fact| match &fact.decoded {
                Some(DecodedRecord::Run(entry)) => Some(entry.record.events.clone()),
                _ => None,
            })
            .flatten()
            .filter(is_start)
            .collect();
        ensure!(starts.is_empty(), "{key}: a start was recorded: {starts:?}");
        self.evidence.journals.extend(journal);
        Ok(())
    }

    async fn assert_recovered(
        &mut self,
        key: &str,
        run: &lash::TurnId,
        descriptor: &IsolatedProcessDescriptor,
        before: Option<(&lash_core::ProcessId, u32)>,
    ) -> Result<()> {
        ensure!(
            descriptor.start_key == h3::isolated_start_key(key)
                && descriptor.boundary == ProcessExecutionBoundary::WorkerProcess
                && descriptor.termination.is_none(),
            "{key}: descriptor {descriptor:?}"
        );
        let journal = self.journal(key, run).await?;
        let starts: Vec<RunEvent> = journal
            .iter()
            .filter_map(|fact| match &fact.decoded {
                Some(DecodedRecord::Run(entry)) => Some(entry.record.events.clone()),
                _ => None,
            })
            .flatten()
            .filter(is_start)
            .collect();
        let start_key = h3::isolated_start_key(key);
        ensure!(
            matches!(
                starts.as_slice(),
                [
                    RunEvent::StartAdmitted { start_key: admitted, .. },
                    RunEvent::StartLaunched { start_key: launched, process_id, .. },
                    RunEvent::StartDischarged { start_key: discharged, cancelled: false, .. },
                ] if *process_id == descriptor.process_id
                    && *admitted == start_key && *launched == start_key && *discharged == start_key
            ),
            "{key}: start events {starts:?}"
        );
        self.evidence.journals.extend(journal);
        let rows = self.rows_for(key)?;
        ensure!(
            rows == vec![Row::drained(
                &descriptor.process_id,
                key,
                self.environment(&rows).await?,
                false
            )],
            "{key}: registry {rows:?}"
        );
        let spawns = self.spawns_for(&descriptor.process_id)?;
        ensure!(
            spawns.len() == 1,
            "{key}: {} worker spawns for one process",
            spawns.len()
        );
        let marker = self
            .marker(&descriptor.process_id)?
            .with_context(|| format!("{key}: worker wrote no marker"))?;
        ensure!(
            marker.pid == spawns[0].pid && alive(marker.pid),
            "{key}: the admitted worker is not the running one"
        );
        ensure!(
            marker.pid != spawns[0].node_pid,
            "{key}: the body has no physical boundary"
        );
        if let Some((process, pid)) = before {
            ensure!(
                descriptor.process_id == *process && marker.pid == pid,
                "{key}: recovery replaced the registered worker"
            );
        }
        ensure!(
            !h3::body_marker(&self.workers, key).exists(),
            "{key}: an ordinary body ran"
        );
        self.evidence
            .stores
            .push(json!({"kind":"isolated_recovered","key":key,
            "descriptor":descriptor,"rows":rows,"spawns":spawns,"marker":marker}));
        Ok(())
    }

    async fn finish(mut self, id: &str, result: Result<()>) -> Result<()> {
        let mut errors = Vec::new();
        if let Err(error) = &result {
            errors.push(format!("{error:#}"));
        }
        // Inspect before teardown: killing the endpoint would itself make
        // healthy process invocations retry their transport.
        let health = async {
            let view = self.case.view()?;
            let prefix = view.service_name("LashProcessWorkflow").replace('\'', "''");
            let processes = self
                .rows()?
                .iter()
                .map(|row| lash_core::ProcessId::parse(&row.process_id))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let node_pid = self
                .serving
                .as_ref()
                .context("no serving node")?
                .pid()?
                .to_string();
            let invocations = loop {
                let invocations: Vec<lash_upgrade_harness::restate_view::ProcessSegment> = view
                    .query(&format!(
                        "SELECT target_service_name AS lane, target_service_key AS key, \
                         id, status, last_failure, retry_count FROM sys_invocation \
                         WHERE target_service_name LIKE '{prefix}%' AND target_handler_name = 'run'"
                    ))
                    .await?;
                let retrying = invocations.iter().any(|segment| {
                    matches!(segment.invocation.status.as_str(), "backing-off" | "paused")
                        || segment.invocation.last_failure.is_some()
                });
                // Running is healthy only after the current node reached the
                // registered engine. A completed cancelled workflow may skip
                // the body altogether. Neither an empty query nor a pending
                // invocation can satisfy this observation.
                let observed = processes.iter().all(|process| {
                    invocations.iter().any(|segment| {
                        segment.key == process.as_str()
                            && (segment.invocation.status == "completed"
                                || std::fs::read_to_string(h3::observer_file(
                                    &self.workers,
                                    process,
                                ))
                                .is_ok_and(|pid| pid == node_pid))
                    })
                });
                if retrying || observed {
                    break invocations;
                }
                ensure!(
                    Instant::now() < self.deadline(),
                    "isolated processes never reached their worker engines: {invocations:?}"
                );
                tokio::time::sleep(POLL).await;
            };
            self.evidence
                .stores
                .push(json!({"kind":"isolated_process_invocations",
                "invocations":invocations.iter().map(|segment| json!({
                    "key":segment.key,"lane":segment.lane,
                    "id":segment.invocation.id,"status":segment.invocation.status,
                    "last_failure":segment.invocation.last_failure,
                    "retry_count":segment.invocation.retry_count
                })).collect::<Vec<_>>()}));
            ensure!(
                invocations.iter().all(|segment| !matches!(
                    segment.invocation.status.as_str(),
                    "backing-off" | "paused"
                ) && segment.invocation.last_failure.is_none()),
                "isolated process workflow is retrying: {invocations:?}"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = health {
            errors.push(format!("process workflow health: {error:#}"));
        }
        match (self.markers(), self.spawns(), self.rows()) {
            (Ok(markers), Ok(spawns), Ok(rows)) => self.evidence.stores.push(
                json!({"kind":"isolated_final_state","markers":markers,"spawns":spawns,"rows":rows}),
            ),
            (markers, spawns, rows) => errors.push(format!(
                "final state read: {:?} {:?} {:?}",
                markers.err(),
                spawns.err(),
                rows.err()
            )),
        }
        let workers: Vec<u32> = self
            .markers()
            .unwrap_or_default()
            .iter()
            .map(|marker| marker.pid)
            .chain(
                self.spawns()
                    .unwrap_or_default()
                    .iter()
                    .map(|spawn| spawn.pid),
            )
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for pid in &workers {
            if alive(*pid) {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
        if let Some(serving) = self.serving.take() {
            let uri = serving.uri().map(str::to_owned);
            match serving.stop() {
                Ok(()) => self.evidence.cleanup.push(CleanupReceipt {
                    resource: "upgrade-node".into(),
                    closed: true,
                    detail: format!("serving node killed and reaped ({uri:?})"),
                }),
                Err(error) => errors.push(format!("node cleanup: {error:#}")),
            }
        }
        let listener = self.bind.clone();
        let closed = tokio::net::TcpStream::connect(&listener).await.is_err();
        self.evidence.cleanup.push(CleanupReceipt {
            resource: format!("listener:{listener}"),
            closed,
            detail: "node endpoint refuses connections".into(),
        });
        // A worker whose node was killed is adopted by the test runner's
        // subreaper (tools/buck2/test_timeout.py), which reaps the test's
        // process group when the test exits; the runner records that reap as
        // `cleanup_complete`. The test proves the worker stopped running.
        for pid in workers {
            let deadline = Instant::now() + Duration::from_secs(30);
            while alive(pid) && Instant::now() < deadline {
                tokio::time::sleep(POLL).await;
            }
            let (closed, detail) = match stat(pid) {
                None => (true, "reaped: absent from /proc".to_owned()),
                Some((state, parent)) => (
                    !alive(pid),
                    format!(
                        "terminated (state {state}); parent {parent} reaps it when the test exits"
                    ),
                ),
            };
            self.evidence.cleanup.push(CleanupReceipt {
                resource: format!("worker:{pid}"),
                closed,
                detail,
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
        write(&directory.join("evidence.json"), &self.evidence)?;
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
        write(
            &directory.join("result.json"),
            &json!({"scenario":id,"variant":"structural-node","selected":counts.selected,
                "executed":counts.executed,"passed":counts.passed,"failed":counts.failed,
                "not_run":counts.not_run,"error":error}),
        )?;
        if let Some(error) = error {
            bail!("{error}");
        }
        counts.reconcile()?;
        println!("{id} structural-node selected=1 executed=1 passed=1 failed=0 not_run=0");
        Ok(())
    }
}

fn artifact(role: &str, path: PathBuf) -> Result<ArtifactIdentity> {
    let sha256 = lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?);
    Ok(ArtifactIdentity {
        role: role.into(),
        path,
        sha256,
        candidate_sha: std::env::var("LASH_E2E_CANDIDATE_SHA")?,
        generation: "candidate".into(),
    })
}

fn write(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The state and parent fields of `/proc/<pid>/stat`, when the pid exists.
fn stat(pid: u32) -> Option<(char, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?.chars().next()?;
    Some((state, fields.next()?.parse().ok()?))
}

fn state(pid: u32) -> Option<char> {
    stat(pid).map(|(state, _)| state)
}

/// Running: present and not a zombie awaiting its reaper.
fn alive(pid: u32) -> bool {
    state(pid).is_some_and(|state| state != 'Z' && state != 'X')
}

/// Reaped: the pid no longer exists, not even as a zombie.
fn gone(pid: u32) -> bool {
    state(pid).is_none()
}
