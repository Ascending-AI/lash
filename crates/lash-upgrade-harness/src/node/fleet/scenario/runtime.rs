//! Owned fleet services. All work facts come from the production hosts,
//! PostgreSQL rows, H2 ledgers and independently queried Restate nodes.
use super::*;
use crate::e2e::cluster::{LocalCluster, NodeReceipt};
use crate::e2e::control::transport::V7Proxy;
use crate::e2e::control::{CleanupReceipt, FaultReceipt, FileBarriers, ProcessReceipt};
use crate::e2e::evidence::{EvidenceReader, RestateEvidenceReader};
use crate::e2e::host::{HostObservation, HostReady};
use crate::harness::ServingNode;
use crate::node::fleet::host::FleetReady;
use crate::node::tools::ToolDelivery;
use crate::restate_view::RestateView;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;

type SharedCluster = Arc<AsyncMutex<LocalCluster>>;
type SharedProxy = Arc<AsyncMutex<Option<V7Proxy>>>;
type Faults = Arc<Mutex<Vec<FaultReceipt>>>;

fn control_socket(directory: &Path, scratch: &Path) -> PathBuf {
    // e2e-gate supplies a fixed-width TMPDIR. Artifact paths can exceed
    // sun_path even when made relative to the fork, so only hash them here.
    // The directory includes the host role; replacements reuse its socket.
    let identity = lash_core::stable_hash::sha256_hex(directory.as_os_str().as_encoded_bytes());
    scratch.join(format!("fleet-{}.sock", &identity[..32]))
}

pub struct RuntimeCluster {
    cluster: SharedCluster,
    faults: Faults,
    directory: PathBuf,
    work: Arc<Mutex<Option<WorkIdentity>>>,
}
impl ClusterControl for RuntimeCluster {
    fn boot<'a>(
        &'a mut self,
        binary: &'a ArtifactIdentity,
        nodes: usize,
        lease: &'a mut CaseLease,
    ) -> Step<'a, ClusterReceipt> {
        Box::pin(async move { self.cluster.lock().await.boot(binary, nodes, lease).await })
    }
    fn leaders(&mut self) -> Step<'_, Vec<LeaderReceipt>> {
        Box::pin(async move { self.cluster.lock().await.leaders().await })
    }
    fn kill<'a>(&'a mut self, node: u32, proof: &'a BarrierProof) -> Step<'a, FaultReceipt> {
        Box::pin(async move {
            let receipt = self.cluster.lock().await.kill(node, proof).await?;
            self.faults
                .lock()
                .map_err(|_| anyhow::anyhow!("fault receipts poisoned"))?
                .push(receipt.clone());
            Ok(receipt)
        })
    }
    fn restart(&mut self, node: u32) -> Step<'_, NodeReceipt> {
        Box::pin(async move { self.cluster.lock().await.restart(node).await })
    }
    fn partition(&mut self, from: u32, to: u32) -> Step<'_, ()> {
        Box::pin(async move {
            let cluster = self.cluster.lock().await;
            let closed = cluster.partition_receipt(from, to).await?;
            ensure!(
                closed > 0,
                "directed cut {from}->{to} missed every established stream"
            );
            let work = self
                .work
                .lock()
                .map_err(|_| anyhow::anyhow!("work identity poisoned"))?
                .clone()
                .context("partition has no admitted work")?;
            let path = self.directory.join(format!("partition-{from}-{to}.json"));
            write(
                &path,
                &serde_json::json!({"from":from,"to":to,"closed_streams":closed}),
            )?;
            let receipt = FaultReceipt {
                fault: Fault::PartitionLink { from, to },
                proof: BarrierProof {
                    barrier: Barrier {
                        work,
                        kind: BarrierKind::TransportConnected,
                    },
                    artifact: path.display().to_string(),
                    journal_index: None,
                },
                target_incarnation: 0,
            };
            self.faults
                .lock()
                .map_err(|_| anyhow::anyhow!("fault receipts poisoned"))?
                .push(receipt);
            Ok(())
        })
    }
    fn heal(&mut self, from: u32, to: u32) -> Step<'_, ()> {
        Box::pin(async move {
            let mut cluster = self.cluster.lock().await;
            cluster.heal(from, to).await?;
            let work = self
                .work
                .lock()
                .map_err(|_| anyhow::anyhow!("work identity poisoned"))?
                .clone()
                .context("heal has no admitted work")?;
            let path = self.directory.join(format!("heal-{from}-{to}.json"));
            write(
                &path,
                &serde_json::json!({"from":from,"to":to,"enabled":true}),
            )?;
            self.faults
                .lock()
                .map_err(|_| anyhow::anyhow!("fault receipts poisoned"))?
                .push(FaultReceipt {
                    fault: Fault::HealLink { from, to },
                    proof: BarrierProof {
                        barrier: Barrier {
                            work,
                            kind: BarrierKind::TransportConnected,
                        },
                        artifact: path.display().to_string(),
                        journal_index: None,
                    },
                    target_incarnation: 0,
                });
            Ok(())
        })
    }
    fn converge(&mut self) -> Step<'_, ClusterReceipt> {
        Box::pin(async move {
            let receipt = self.cluster.lock().await.converge().await?;
            write(&self.directory.join("cluster-convergence.json"), &receipt)?;
            Ok(receipt)
        })
    }
    fn observe_leg<'a>(&'a mut self, directory: &'a Path) -> Step<'a, serde_json::Value> {
        Box::pin(async move { self.cluster.lock().await.observe_leg(directory).await })
    }
    fn finish(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move { self.cluster.lock().await.finish().await })
    }
}

#[derive(Clone)]
struct HostConfig {
    scenario: String,
    session: String,
    store: String,
    directory: PathBuf,
    barriers: PathBuf,
    ingress: String,
    admin: String,
    authority: String,
    namespace: String,
    publication_cut: bool,
}
struct Client {
    config: Option<HostConfig>,
    process: Option<ServingNode>,
    ready: Option<FleetReady>,
    observations: Vec<HostObservation>,
    deadline: Instant,
    work: Arc<Mutex<Option<WorkIdentity>>>,
}
impl Client {
    fn new(deadline: Instant, work: Arc<Mutex<Option<WorkIdentity>>>) -> Self {
        Self {
            config: None,
            process: None,
            ready: None,
            observations: Vec::new(),
            deadline,
            work,
        }
    }
    async fn request(&mut self, command: HostCommand) -> Result<HostObservation> {
        self.process
            .as_mut()
            .context("host not booted")?
            .assert_running()?;
        let socket = tokio::net::UnixStream::connect(
            &self
                .ready
                .as_ref()
                .context("host has no readiness receipt")?
                .control,
        )
        .await?;
        let (input, mut output) = socket.into_split();
        let mut bytes = serde_json::to_vec(&command)?;
        bytes.push(b'\n');
        output.write_all(&bytes).await?;
        output.shutdown().await?;
        let mut line = String::new();
        tokio::time::timeout_at(
            self.deadline.into(),
            BufReader::new(input).read_line(&mut line),
        )
        .await
        .context("host command deadline")??;
        let reply: std::result::Result<HostObservation, String> = serde_json::from_str(&line)
            .with_context(|| format!("host control response {line:?}"))?;
        let reply = reply.map_err(anyhow::Error::msg)?;
        if !reply.work.run.is_empty() {
            let mut work = self
                .work
                .lock()
                .map_err(|_| anyhow::anyhow!("work identity poisoned"))?;
            if let Some(prior) = work.as_ref() {
                assert_same_owner(prior, &reply.work)?;
            } else {
                *work = Some(reply.work.clone());
            }
            self.observations.push(reply.clone());
            write(
                &self
                    .config
                    .as_ref()
                    .context("host configuration missing")?
                    .directory
                    .join("observations.json"),
                &self.observations,
            )?;
        }
        Ok(reply)
    }
    fn remove_control_socket(&self) -> Result<()> {
        if let Some(ready) = &self.ready {
            match std::fs::remove_file(&ready.control) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    async fn setup(&mut self, action: &str, input: serde_json::Value) -> Result<serde_json::Value> {
        Ok(self
            .request(HostCommand::Process {
                action: action.into(),
                input,
            })
            .await?
            .output)
    }
}
impl HostAdapter for Client {
    fn boot<'a>(
        &'a mut self,
        artifact: &'a ArtifactIdentity,
        lease: &'a mut CaseLease,
    ) -> Step<'a, HostReady> {
        Box::pin(async move {
            artifact.verify()?;
            let config = self.config.as_ref().context("host configuration missing")?;
            std::fs::create_dir_all(&config.directory)?;
            let ready_file = config.directory.join("ready.json");
            let log = config.directory.join("host.log");
            let mut command = Command::new(&artifact.path);
            command
                .args(["fleet-serve", "--store", &config.store, "--data-dir"])
                .arg(lease.directory.join("attachments"))
                .args([
                    "--ingress-url",
                    &config.ingress,
                    "--admin-url",
                    &config.admin,
                    "--authority",
                    &config.authority,
                    "--namespace",
                    &config.namespace,
                    "--scenario",
                    &config.scenario,
                    "--session",
                    &config.session,
                    "--directory",
                ])
                .arg(&config.directory)
                .arg("--barrier-directory")
                .arg(&config.barriers)
                .args(["--bind", "127.0.0.1:0", "--control-socket"])
                .arg(control_socket(&config.directory, &std::env::temp_dir()))
                .args(["--ready-file"])
                .arg(&ready_file)
                .args(["--timeout-secs", "240"]);
            if config.publication_cut {
                command.arg("--publication-cut");
            }
            self.process = Some(ServingNode::spawn(&mut command, &log)?);
            let pid = self
                .process
                .as_ref()
                .context("host process missing")?
                .pid()?;
            let process = ProcessReceipt {
                role: format!(
                    "fleet-{}",
                    config
                        .directory
                        .file_name()
                        .context("host directory has no name")?
                        .to_string_lossy()
                ),
                pid,
                incarnation: 1,
                log: log.display().to_string(),
            };
            lease.processes.push(process.clone());
            loop {
                self.process
                    .as_mut()
                    .context("host process missing")?
                    .assert_running()?;
                match std::fs::read(&ready_file) {
                    Ok(bytes) => {
                        self.ready = Some(serde_json::from_slice(&bytes)?);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                ensure!(
                    Instant::now() < self.deadline,
                    "fleet host did not become ready"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok(HostReady {
                endpoint: self
                    .ready
                    .as_ref()
                    .context("readiness missing")?
                    .serving
                    .uri
                    .clone(),
                process,
                protocol: 0,
            })
        })
    }
    fn command<'a>(&'a mut self, command: HostCommand) -> Step<'a, HostObservation> {
        Box::pin(async move { self.request(command).await })
    }
    fn transcript(&self) -> Result<Vec<HostObservation>> {
        Ok(self.observations.clone())
    }
    fn stop(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            let Some(mut process) = self.process.take() else {
                return Ok(Vec::new());
            };
            if process.is_reaped() {
                self.remove_control_socket()?;
                return Ok(Vec::new());
            }
            let pid = process.pid()?;
            let sent = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()?;
            let result = if sent.success() {
                process
                    .wait_success(Instant::now() + Duration::from_secs(15))
                    .await
            } else {
                Err(anyhow::anyhow!("SIGTERM failed for owned host {pid}"))
            };
            if result.is_err() && !process.is_reaped() {
                process.kill_and_reap()?;
            }
            self.remove_control_socket()?;
            Ok(vec![CleanupReceipt {
                resource: format!("host:{pid}"),
                closed: result.is_ok(),
                detail: result
                    .err()
                    .map(|error| format!("{error:#}"))
                    .unwrap_or_else(|| "orderly shutdown and reap".into()),
            }])
        })
    }
}

pub struct RuntimeControl {
    cluster: SharedCluster,
    proxy: SharedProxy,
    barriers: PathBuf,
    namespace: String,
    deadline: Instant,
    faults: Faults,
    observed: Vec<BarrierProof>,
}
impl Control for RuntimeControl {
    fn await_barrier<'a>(&'a mut self, barrier: &'a Barrier) -> Step<'a, BarrierProof> {
        Box::pin(async move {
            let barriers = FileBarriers::new(self.barriers.clone(), self.deadline)?;
            let proof = if barrier.kind.durable() {
                let cluster = self.cluster.lock().await;
                let nodes = cluster.nodes();
                let node = nodes.first().context("cluster not booted")?;
                let view = RestateView::new(&node.admin_url, &self.namespace)?;
                drop(cluster);
                let mut reader = RestateEvidenceReader::new("fleet".into(), view, 7);
                reader.bind(&barrier.work, barrier.work.segment.clone())?;
                let mut controller =
                    crate::e2e::control::CoreControl::new(barriers, Box::new(reader));
                controller.await_barrier(barrier).await?
            } else {
                barriers.await_proof(barrier).await?
            };
            self.observed.push(proof.clone());
            Ok(proof)
        })
    }
    fn inject<'a>(&'a mut self, fault: Fault, proof: &'a BarrierProof) -> Step<'a, FaultReceipt> {
        Box::pin(async move {
            ensure!(
                self.observed
                    .iter()
                    .any(|observed| observed.barrier == proof.barrier
                        && observed.artifact == proof.artifact
                        && observed.journal_index == proof.journal_index),
                "fault lacks exact observed cut"
            );
            let Fault::DropConnection { target } = &fault else {
                anyhow::bail!("fleet control does not own {fault:?}");
            };
            ensure!(target == "fleet-primary", "unknown connection target");
            let mut proxy = self.proxy.lock().await;
            let proxy = proxy.as_mut().context("publication proxy not owned")?;
            let closed = proxy.disconnect(self.deadline).await?;
            ensure!(
                closed > 0,
                "disconnect missed every established service stream"
            );
            let path = self.barriers.join("disconnect.json");
            write(
                &path,
                &serde_json::json!({"closed_streams":closed,"endpoint":proxy.endpoint}),
            )?;
            let receipt = FaultReceipt {
                fault,
                proof: proof.clone(),
                target_incarnation: 1,
            };
            self.faults
                .lock()
                .map_err(|_| anyhow::anyhow!("fault receipts poisoned"))?
                .push(receipt.clone());
            Ok(receipt)
        })
    }
    fn tool<'a>(&'a mut self, command: ToolControl) -> Step<'a, ()> {
        Box::pin(async move {
            let barriers = FileBarriers::new(self.barriers.clone(), self.deadline)?;
            match command {
                ToolControl::Hold(barrier) => barriers.hold(&barrier),
                ToolControl::Release(barrier) => barriers.release(&barrier),
                _ => anyhow::bail!("fleet has no source-resolution fixture"),
            }
        })
    }
}

pub struct NativeRoute {
    role: &'static str,
    node: u32,
    proxy: V7Proxy,
}

struct RuntimeFixture {
    cluster: SharedCluster,
    proxy: SharedProxy,
    faults: Faults,
    primary: Client,
    follower: Client,
    directory: PathBuf,
    barriers: PathBuf,
    namespace: String,
    deadline: Instant,
    base: u16,
    binary: ArtifactIdentity,
    host: ArtifactIdentity,
    pool: Option<sqlx::PgPool>,
    stores: Option<Arc<dyn lash::StoreSet>>,
    container: Option<String>,
    routes: Vec<NativeRoute>,
}
impl RuntimeFixture {
    async fn postgres(&mut self, lease: &mut CaseLease) -> Result<String> {
        let name = format!("lash-{}-pg", lease.namespace);
        let port = self
            .base
            .checked_add(40)
            .context("PostgreSQL port overflow")?;
        // The case owns only this container and this loopback port.
        drop(TcpListener::bind(("127.0.0.1", port))?);
        let output = tokio::process::Command::new("docker")
            .args([
                "run",
                "--detach",
                "--name",
                &name,
                "--label",
                "lash.e2e=owned",
                "-p",
                &format!("127.0.0.1:{port}:5432"),
                "-e",
                "POSTGRES_PASSWORD=e2e",
                "-e",
                "POSTGRES_DB=lash",
                "postgres:16",
            ])
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "private PostgreSQL boot: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.container = Some(name.clone());
        lease.ports.push(port);
        write(
            &self.directory.join("postgres-lease.json"),
            &serde_json::json!({"container":name,"id":String::from_utf8(output.stdout)?.trim(),"port":port}),
        )?;
        let url = format!("postgres://postgres:e2e@127.0.0.1:{port}/lash");
        loop {
            match sqlx::postgres::PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(Duration::from_secs(1))
                .connect(&url)
                .await
            {
                Ok(pool) => {
                    let version: String = sqlx::query_scalar("SHOW server_version")
                        .fetch_one(&pool)
                        .await?;
                    ensure!(
                        version.starts_with("16."),
                        "fleet requires PG16, got {version}"
                    );
                    self.pool = Some(pool);
                    break;
                }
                Err(error) => {
                    ensure!(
                        Instant::now() < self.deadline,
                        "private PostgreSQL did not answer: {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
        sqlx::raw_sql(lash_postgres_store::PostgresStorage::schema_ddl())
            .execute(
                self.pool
                    .as_ref()
                    .context("private PostgreSQL pool missing")?,
            )
            .await?;
        write(
            &self.directory.join("postgres-schema.json"),
            &serde_json::json!({"ddl_sha256":lash_core::stable_hash::sha256_hex(lash_postgres_store::PostgresStorage::schema_ddl().as_bytes())}),
        )?;
        let storage = lash_postgres_store::PostgresStorage::connect(&url).await?;
        let attachments = self.directory.join("attachments");
        std::fs::create_dir_all(&attachments)?;
        self.stores = Some(Arc::new(lash_postgres_store::PostgresStoreSet::new(
            &storage,
            Arc::new(lash::persistence::FileAttachmentStore::new(attachments)),
        )));
        lease.postgres_url = Some(url.clone());
        Ok(url)
    }
    fn deliveries(&self) -> Result<Vec<ToolDelivery>> {
        let mut deliveries = Vec::new();
        for host in ["primary", "follower"] {
            let path = self.directory.join(host).join("deliveries.jsonl");
            if path.exists() {
                deliveries.extend(crate::node::tools::deliveries(&path)?);
            }
        }
        Ok(deliveries)
    }
}
impl FleetFixture for RuntimeFixture {
    fn prepare<'a>(&'a mut self, scenario: Scenario, lease: &'a mut CaseLease) -> Step<'a, ()> {
        Box::pin(async move {
            let spec = scenario.spec(vec![self.binary.clone(), self.host.clone()]);
            // F01/P58/B01 have landed. Preserve their exact landing evidence;
            // selection cannot be made available by deleting an unknown gate.
            let required = match scenario {
                Scenario::LeaderLoss | Scenario::MinorityPartition => vec!["FIG-4894"],
                Scenario::TerminalPublication | Scenario::TerminalRedrive => {
                    vec!["FIG-4878", "FIG-4739"]
                }
            };
            ensure!(
                spec.requires == required,
                "fleet dependency manifest changed"
            );
            let mut selected = spec.clone();
            let mut landings = Vec::new();
            for requirement in &selected.requires {
                // B01's authorized landed commit has a Part-of trailer;
                // identify that exact main outcome by its canonical subject.
                let landing = if requirement == "FIG-4739" {
                    "^Turn continuations publish complete Run state with atomic material ownership$"
                        .to_owned()
                } else {
                    format!("^Closes {requirement}$")
                };
                let output = Command::new("git")
                    .args([
                        "log",
                        "origin/main",
                        "-1",
                        "--format=%H",
                        "--grep",
                        &landing,
                    ])
                    .output()?;
                ensure!(
                    output.status.success(),
                    "read {requirement} main landing failed"
                );
                let tip = String::from_utf8(output.stdout)?.trim().to_owned();
                ensure!(
                    !tip.is_empty(),
                    "{requirement} has no canonical main landing"
                );
                ensure!(
                    Command::new("git")
                        .args(["merge-base", "--is-ancestor", &tip, "HEAD"])
                        .status()?
                        .success(),
                    "{requirement} prerequisite {tip} has not landed in candidate"
                );
                landings.push((requirement.clone(), tip));
            }
            selected.requires.clear();
            selected.validate()?;
            write(
                &self.directory.join("manifest.json"),
                &serde_json::json!({"case":selected,"prerequisites":landings,"note":if matches!(scenario,Scenario::TerminalPublication|Scenario::TerminalRedrive){"Ruling 13951: terminal publication is not a quiet point; drain cannot advance the fence. Disconnect and SIGKILL replay keep the original segment and journaled seal. Higher-fence stale refusal belongs to store laws, not this in-flight case."}else{"real PG16, real three-node Restate, two worker processes"}}),
            )?;
            let url = self.postgres(lease).await?;
            let boot = self
                .cluster
                .lock()
                .await
                .boot(&self.binary, spec.restate_nodes, lease)
                .await?;
            write(&self.directory.join("cluster-boot.json"), &boot)?;
            let mut ingress = boot.nodes[0].ingress_url.clone();
            let mut admin = boot.nodes[0].admin_url.clone();
            if matches!(scenario, Scenario::MinorityPartition) {
                for (role, endpoint) in [("ingress", &mut ingress), ("admin", &mut admin)] {
                    let upstream: SocketAddr = endpoint.trim_start_matches("http://").parse()?;
                    let proxy = V7Proxy::start(
                        TcpListener::bind("127.0.0.1:0")?,
                        upstream,
                        self.directory.join(format!("native-{role}-route")),
                        self.deadline,
                        Vec::new(),
                    )
                    .await?;
                    *endpoint = proxy.endpoint.clone();
                    self.routes.push(NativeRoute {
                        role,
                        node: boot.nodes[0].node,
                        proxy,
                    });
                }
            }
            for (name, client) in [
                ("primary", &mut self.primary),
                ("follower", &mut self.follower),
            ] {
                client.config = Some(HostConfig {
                    scenario: spec.id.clone(),
                    session: format!("{}-fleet", lease.namespace),
                    store: url.clone(),
                    directory: self.directory.join(name),
                    barriers: self.barriers.clone(),
                    ingress: ingress.clone(),
                    admin: admin.clone(),
                    authority: lease.authority.clone(),
                    namespace: lease.namespace.clone(),
                    publication_cut: name == "primary"
                        && matches!(
                            scenario,
                            Scenario::TerminalPublication | Scenario::TerminalRedrive
                        ),
                });
                client.boot(&self.host, lease).await?;
            }
            let primary = self
                .primary
                .ready
                .as_ref()
                .context("primary not ready")?
                .serving
                .uri
                .clone();
            let follower = self
                .follower
                .ready
                .as_ref()
                .context("follower not ready")?
                .serving
                .uri
                .clone();
            self.follower
                .setup("register-uri", serde_json::json!(follower))
                .await?;
            if matches!(
                scenario,
                Scenario::TerminalPublication | Scenario::TerminalRedrive
            ) {
                let upstream: SocketAddr = primary.trim_start_matches("http://").parse()?;
                let proxy = V7Proxy::start(
                    TcpListener::bind("127.0.0.1:0")?,
                    upstream,
                    self.directory.join("proxy"),
                    self.deadline,
                    Vec::new(),
                )
                .await?;
                let endpoint = proxy.endpoint.clone();
                *self.proxy.lock().await = Some(proxy);
                self.primary
                    .setup("register-uri", serde_json::json!(endpoint))
                    .await?;
            } else {
                self.primary
                    .setup("register-uri", serde_json::json!(primary))
                    .await?;
            }
            if matches!(scenario, Scenario::MinorityPartition) {
                let a = self
                    .primary
                    .setup("receiver-register", serde_json::Value::Null)
                    .await?;
                let b = self.follower.setup("receiver-bind", a.clone()).await?;
                ensure!(
                    a == b,
                    "two fleet hosts bound different admitted receiver receipts"
                );
                write(&self.directory.join("receiver-start.json"), &a)?;
            }
            Ok(())
        })
    }
    fn primary(&mut self) -> &mut dyn HostAdapter {
        &mut self.primary
    }
    fn follower(&mut self) -> &mut dyn HostAdapter {
        &mut self.follower
    }
    fn observed_work(&mut self) -> Step<'_, Option<WorkIdentity>> {
        Box::pin(async move {
            if let Some(observation) = self.primary.observations.first() {
                return Ok(Some(observation.work.clone()));
            }
            let Some(delivery) = self.deliveries()?.into_iter().next() else {
                return Ok(None);
            };
            let run = delivery
                .logical_run
                .context("actual delivered body has no logical Run")?;
            let session = lash::SessionId::parse(format!("{}-fleet", self.namespace))?;
            let inputs: Vec<String> = sqlx::query_scalar(
                "SELECT input_id FROM lash_session_run_inputs WHERE session_id=$1 AND run=$2",
            )
            .bind(session.as_str())
            .bind(run.as_str())
            .fetch_all(self.pool.as_ref().context("observed body has no PG pool")?)
            .await?;
            ensure!(
                inputs.len() == 1,
                "actual body does not retain one input binding"
            );
            let nodes = self.cluster.lock().await.nodes();
            let node = nodes.first().context("observed body has no cluster")?;
            let mut reader = RestateEvidenceReader::new(
                "fleet".into(),
                RestateView::new(&node.admin_url, &self.namespace)?,
                7,
            );
            Ok(Some(
                reader
                    .bind_public_run(
                        self.stores
                            .as_ref()
                            .context("observed body has no PG stores")?
                            .session_store_factory()
                            .as_ref(),
                        &session,
                        &run,
                        inputs[0].clone(),
                    )
                    .await?,
            ))
        })
    }
    fn barrier<'a>(
        &'a mut self,
        work: &'a WorkIdentity,
        label: &'a str,
        kind: BarrierKind,
    ) -> Step<'a, Barrier> {
        Box::pin(async move {
            if matches!(
                kind,
                BarrierKind::PublicationRequest | BarrierKind::SuccessorFence
            ) {
                return Ok(Barrier {
                    work: work.clone(),
                    kind,
                });
            }
            loop {
                self.primary
                    .process
                    .as_mut()
                    .context("primary not booted")?
                    .assert_running()?;
                let deliveries = self.deliveries()?;
                if let Some(delivery) = deliveries.iter().find(|delivery| {
                    delivery.label == label
                        && delivery.logical_run.as_ref().map(|run| run.as_str())
                            == Some(work.run.as_str())
                }) {
                    let mut work = work.clone();
                    work.call = Some(delivery.call_id.to_string());
                    work.ordinal = Some(delivery.ordinal);
                    return Ok(Barrier { work, kind });
                }
                ensure!(
                    Instant::now() < self.deadline,
                    "tool {label} did not enter on the accepted Run"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
    }
    fn capture<'a>(
        &'a mut self,
        work: &'a WorkIdentity,
        excluded_node: Option<u32>,
    ) -> Step<'a, Evidence> {
        Box::pin(async move {
            let nodes = self.cluster.lock().await.nodes().to_vec();
            let mut evidence = Evidence::empty("fleet".into());
            evidence.artifacts = vec![self.binary.clone(), self.host.clone()];
            evidence.native_records = crate::node::fleet::observation::read(&[
                self.directory.join("primary"),
                self.directory.join("follower"),
            ])?;
            evidence.faults = self
                .faults
                .lock()
                .map_err(|_| anyhow::anyhow!("fault receipts poisoned"))?
                .clone();
            let after_fault = !evidence.faults.is_empty();
            capture_journals(
                &mut evidence,
                &nodes,
                &self.namespace,
                work,
                excluded_node,
                self.deadline,
            )
            .await?;
            evidence.outputs.extend(self.primary.transcript()?);
            evidence.outputs.extend(self.follower.transcript()?);
            evidence.effects = self
                .deliveries()?
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<_, _>>()?;
            if self
                .primary
                .config
                .as_ref()
                .map(|config| config.scenario.as_str())
                == Some("S15")
            {
                evidence.effects.push(
                    self.follower
                        .setup("receiver-events", serde_json::Value::Null)
                        .await?,
                );
            }
            let pool = self.pool.as_ref().context("fleet has no PG pool")?;
            let session = format!("{}-fleet", self.namespace);
            let head:serde_json::Value=sqlx::query_scalar("SELECT json_build_object('shift_epoch',m.shift_epoch,'head_revision',h.head_revision,'head',r.head_json)::jsonb FROM lash_session_meta m JOIN lash_session_head h USING(session_id) JOIN lash_session_revisions r USING(session_id,head_revision) WHERE m.session_id=$1").bind(&session).fetch_one(pool).await?;
            evidence.stores.push(head);
            if after_fault {
                let store = self
                    .stores
                    .as_ref()
                    .context("fleet stores missing")?
                    .session_store_factory();
                let session_id = lash::SessionId::parse(&session)?;
                let run = lash::TurnId::parse(&work.run)?;
                ensure!(
                    store
                        .run_of_input(&session_id, &lash::InputId::parse(&work.ingress)?)
                        .await?
                        == Some(run.clone()),
                    "fault changed accepted input Run binding"
                );
                ensure!(
                    store.run_executor(&session_id, &run).await?.is_some(),
                    "fault lost admitted Run executor"
                );
                let window = store
                    .load_session_window(&session_id, lash_core::store::WindowSelector::Current)
                    .await?
                    .context("fault lost session head")?;
                let state = lash_core::store::window_state(window, store.fleet_format())?.state;
                evidence
                    .stores
                    .push(serde_json::json!({"kind":"authoritative_session_state", "state":state}));
                // Diagnostic captures can precede Attach while the Run is pending.
                // Settlement assertions require this fact once a terminal exists.
                if let Some(terminal) = store.run_terminal(&session_id, &run).await? {
                    evidence.stores.push(
                        serde_json::json!({"kind":"authoritative_run_terminal", "terminal":terminal}),
                    );
                }
            }
            Ok(evidence)
        })
    }
    fn partition_for<'a>(&'a mut self, work: &'a WorkIdentity) -> Step<'a, u32> {
        Box::pin(async move {
            Ok(self
                .cluster
                .lock()
                .await
                .leader_for_invocation(&work.segment)
                .await?
                .partition)
        })
    }
    fn await_leader<'a>(&'a mut self, previous: &'a LeaderReceipt) -> Step<'a, LeaderReceipt> {
        Box::pin(async move {
            let cluster = self.cluster.lock().await;
            let successor = cluster.await_leader_change(previous).await?;
            let nodes = cluster.nodes();
            let majority = nodes
                .iter()
                .find(|node| node.node == successor.node)
                .context("observed majority leader is not owned")?;
            for route in &mut self.routes {
                if route.node == previous.node {
                    let endpoint = match route.role {
                        "ingress" => &majority.ingress_url,
                        "admin" => &majority.admin_url,
                        _ => anyhow::bail!("unknown native frontend route"),
                    };
                    let next: SocketAddr = endpoint.trim_start_matches("http://").parse()?;
                    let old = route.proxy.replace_upstream(next)?;
                    // Engines retain ingress pools; close their established
                    // streams so the next request uses the majority. Worker
                    // admin observations create and drop their own clients.
                    let closed = if route.role == "ingress" {
                        Some(route.proxy.disconnect(self.deadline).await?)
                    } else {
                        None
                    };
                    write(
                        &self
                            .directory
                            .join(format!("native-{}-majority.json", route.role)),
                        &serde_json::json!({"previous_node":route.node,"next_node":majority.node,
                        "old_upstream":old,"next_upstream":next,"closed_streams":closed,
                        "stable_endpoint":route.proxy.endpoint,"observed_leader":&successor}),
                    )?;
                    route.node = majority.node;
                }
                ensure!(
                    route.node != previous.node,
                    "native frontend still reaches isolated minority"
                );
            }
            Ok(successor)
        })
    }
    fn snapshot<'a>(&'a mut self, work: &'a WorkIdentity) -> Step<'a, FleetSnapshot> {
        Box::pin(async move {
            let snapshot = FleetSnapshot::read(
                self.pool.as_ref().context("PG pool missing")?,
                self.stores
                    .as_ref()
                    .context("PG stores missing")?
                    .session_store_factory()
                    .as_ref(),
                &lash::SessionId::parse(format!("{}-fleet", self.namespace))?,
                &lash::TurnId::parse(&work.run)?,
            )
            .await?;
            write(&self.directory.join("store-snapshot.json"), &snapshot)?;
            Ok(snapshot)
        })
    }
    fn publication_epoch(&self) -> Result<u64> {
        let request: serde_json::Value = serde_json::from_slice(&std::fs::read(
            self.directory.join("primary/publication-request.json"),
        )?)?;
        request["fence"]["epoch"]
            .as_u64()
            .context("held publication has no observed fence epoch")
    }
    fn kill_publication_host<'a>(&'a mut self, proof: &'a BarrierProof) -> Step<'a, ()> {
        Box::pin(async move {
            ensure!(
                proof.barrier.kind == BarrierKind::PublicationRequest
                    && std::path::Path::new(&proof.artifact).is_file(),
                "SIGKILL lacks retained terminal request"
            );
            let mut proxy = self.proxy.lock().await;
            let proxy = proxy.as_mut().context("service proxy not owned")?;
            let upstream: SocketAddr = self
                .follower
                .ready
                .as_ref()
                .context("follower not ready")?
                .serving
                .uri
                .trim_start_matches("http://")
                .parse()?;
            proxy.replace_upstream(upstream)?;
            let process = self
                .primary
                .process
                .as_mut()
                .context("primary process not owned")?;
            let pid = process.pid()?;
            process.kill_and_reap()?;
            let closed = proxy.disconnect(self.deadline).await?;
            write(
                &self.directory.join("sigkill.json"),
                &serde_json::json!({"pid":pid,"reaped":process.is_reaped(),"closed_streams":closed,"same_endpoint":proxy.endpoint,"next_upstream":upstream}),
            )?;
            self.faults
                .lock()
                .map_err(|_| anyhow::anyhow!("fault receipts poisoned"))?
                .push(FaultReceipt {
                    fault: Fault::KillHost {
                        target: "fleet-primary".into(),
                    },
                    proof: proof.clone(),
                    target_incarnation: 1,
                });
            Ok(())
        })
    }
    fn drain_complete(&mut self) -> Step<'_, ()> {
        Box::pin(async move {
            loop {
                let status = self
                    .follower
                    .setup("drain-status", serde_json::Value::Null)
                    .await?;
                write(&self.directory.join("drain-status.json"), &status)?;
                if status["drained"] == true {
                    return Ok(());
                }
                ensure!(
                    Instant::now() < self.deadline,
                    "terminal settled but generation drain stayed pending"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
    }
    fn finish(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            let mut cleanup = Vec::new();
            for client in [&mut self.primary, &mut self.follower] {
                match client.stop().await {
                    Ok(receipts) => cleanup.extend(receipts),
                    Err(error) => cleanup.push(CleanupReceipt {
                        resource: "fleet-host".into(),
                        closed: false,
                        detail: format!("{error:#}"),
                    }),
                }
            }
            if let Some(mut proxy) = self.proxy.lock().await.take() {
                let result = proxy.finish().await;
                cleanup.push(CleanupReceipt {
                    resource: "service-proxy".into(),
                    closed: result.is_ok(),
                    detail: result
                        .err()
                        .map(|error| format!("{error:#}"))
                        .unwrap_or_else(|| "all owned streams closed".into()),
                });
            }
            for route in &mut self.routes {
                let result = route.proxy.finish().await;
                cleanup.push(CleanupReceipt {
                    resource: format!("native-{}-route", route.role),
                    closed: result.is_ok(),
                    detail: result
                        .err()
                        .map(|error| format!("{error:#}"))
                        .unwrap_or_else(|| "all owned frontend streams closed".into()),
                });
            }
            if let Some(pool) = self.pool.take() {
                pool.close().await;
            }
            self.stores.take();
            if let Some(container) = self.container.take() {
                let logs = tokio::process::Command::new("docker")
                    .args(["logs", &container])
                    .output()
                    .await?;
                std::fs::write(self.directory.join("postgres.log"), logs.stdout)?;
                let removed = tokio::process::Command::new("docker")
                    .args(["rm", "--force", "--volumes", &container])
                    .output()
                    .await?;
                cleanup.push(CleanupReceipt {
                    resource: container,
                    closed: removed.status.success(),
                    detail: String::from_utf8_lossy(&removed.stderr).into_owned(),
                });
            }
            Ok(cleanup)
        })
    }
}

pub async fn run(scenario: Scenario, permutation: Permutation) -> Result<()> {
    let name = match scenario {
        Scenario::LeaderLoss => "s14",
        Scenario::MinorityPartition => "s15",
        Scenario::TerminalPublication => "s16",
        Scenario::TerminalRedrive => "s16-sigkill",
    };
    let name = match permutation.leg {
        Leg::Live => name.to_owned(),
        Leg::Replay => format!("{name}-replay"),
    };
    run_named(scenario, &name, permutation.leg).await
}

/// Compose an existing fleet subcase under the caller's unique case slug.
/// The subcase still owns its services, barriers, receipts and cleanup.
pub async fn run_named(scenario: Scenario, name: &str, leg: Leg) -> Result<()> {
    let (mut lease, cluster, binary) = setup(name, leg)?;
    lease.deadline = Instant::now() + Duration::from_secs(240);
    let host = artifact(
        "upgrade-node",
        PathBuf::from(std::env::var("LASH_UPGRADE_NODE_N")?),
    )?;
    let cluster = Arc::new(AsyncMutex::new(cluster));
    let proxy = Arc::new(AsyncMutex::new(None));
    let faults = Arc::new(Mutex::new(Vec::new()));
    let work = Arc::new(Mutex::new(None));
    let barriers = lease.directory.join("barriers");
    let mut cluster_control = RuntimeCluster {
        cluster: cluster.clone(),
        faults: faults.clone(),
        directory: lease.directory.clone(),
        work: work.clone(),
    };
    let mut control = RuntimeControl {
        cluster: cluster.clone(),
        proxy: proxy.clone(),
        barriers: barriers.clone(),
        namespace: lease.namespace.clone(),
        deadline: lease.deadline,
        faults: faults.clone(),
        observed: Vec::new(),
    };
    let mut fixture = RuntimeFixture {
        cluster,
        proxy,
        faults,
        primary: Client::new(lease.deadline, work.clone()),
        follower: Client::new(lease.deadline, work),
        directory: lease.directory.clone(),
        barriers,
        namespace: lease.namespace.clone(),
        deadline: lease.deadline,
        base: std::env::var("LASH_E2E_PORT_BASE")?.parse()?,
        binary,
        host,
        pool: None,
        stores: None,
        container: None,
        routes: Vec::new(),
    };
    let result = execute(
        scenario,
        &mut lease,
        &mut cluster_control,
        &mut control,
        &mut fixture,
    )
    .await;
    let executed =
        usize::from(!fixture.primary.observations.is_empty() || !fixture.deliveries()?.is_empty());
    let receipt = crate::e2e::evidence::CaseReceipt {
        evidence: match &result {
            Ok(proof) => {
                let mut evidence = proof.after.clone();
                evidence.case = name.into();
                evidence
            }
            Err(_) => {
                let mut evidence = Evidence::empty(name.into());
                evidence.artifacts = vec![fixture.binary.clone(), fixture.host.clone()];
                evidence.cleanup = lease.cleanup.clone();
                evidence
            }
        },
        verdict: match &result {
            Ok(_) => crate::e2e::evidence::Verdict::Passed,
            Err(error) if executed == 0 => crate::e2e::evidence::Verdict::NotRun {
                reason: format!("{error:#}"),
            },
            Err(error) => crate::e2e::evidence::Verdict::Failed {
                reason: format!("{error:#}"),
            },
        },
    };
    let counts = receipt.write(&lease.directory)?;
    write(&lease.directory.join("counts.json"), &counts)?;
    println!(
        "{name} selected=1 executed={} passed={} failed={} not_run={}",
        counts.executed, counts.passed, counts.failed, counts.not_run
    );
    result?;
    counts.reconcile()
}

fn write(path: &std::path::Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn artifact(role: &str, path: PathBuf) -> Result<ArtifactIdentity> {
    Ok(ArtifactIdentity {
        role: role.into(),
        sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?),
        path,
        candidate_sha: std::env::var("LASH_E2E_CANDIDATE_SHA")?,
        generation: "candidate".into(),
    })
}
fn setup(name: &str, leg: Leg) -> Result<(CaseLease, LocalCluster, ArtifactIdentity)> {
    let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
    std::fs::create_dir_all(&root)?;
    let deadline = Instant::now() + Duration::from_secs(240);
    let lease = CaseLease::new(name, root.join(name), deadline)?;
    let base: u16 = std::env::var("LASH_E2E_PORT_BASE")?.parse()?;
    let binary = artifact(
        "restate-server",
        PathBuf::from(std::env::var("LASH_RESTATE_SERVER_BIN")?),
    )?;
    Ok((
        lease,
        LocalCluster::new(base, deadline).with_leg(leg),
        binary,
    ))
}

// Query each available member independently, retaining its admin provenance.
async fn capture_journals(
    evidence: &mut Evidence,
    nodes: &[NodeReceipt],
    namespace: &str,
    work: &WorkIdentity,
    excluded_node: Option<u32>,
    deadline: Instant,
) -> Result<()> {
    for node in nodes.iter().filter(|node| Some(node.node) != excluded_node) {
        let mut reader = RestateEvidenceReader::new(
            evidence.case.clone(),
            RestateView::new(&node.admin_url, namespace)?,
            7,
        );
        reader.bind(work, work.segment.clone())?;
        loop {
            match reader.collect(work).await {
                Ok(collected) => {
                    evidence.journals.extend(collected.journals);
                    break;
                }
                Err(error) => {
                    // A healed member can still be moving its partition store.
                    // Retry that member, preserving its independent provenance;
                    // other failures never count as an absent or drained journal.
                    let moving = matches!(
                        error.downcast_ref::<lash_restate::RestateHttpError>(),
                        Some(lash_restate::RestateHttpError::Status { status: 500, body, .. })
                            if serde_json::from_str::<serde_json::Value>(body).is_ok_and(|body|
                                body["message"].as_str().is_some_and(|message|
                                    message.contains("expecting a partition store")))
                    );
                    if !moving || Instant::now() >= deadline {
                        return Err(error);
                    }
                    tokio::time::sleep_until(
                        (Instant::now() + Duration::from_millis(100))
                            .min(deadline)
                            .into(),
                    )
                    .await;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::AsyncReadExt;

    /// FIG-5110: artifact depth and UTF-8 byte length cannot break fleet control.
    #[test]
    fn fleet_control_socket_binds_independently_of_artifact_path() -> Result<()> {
        let root = std::env::current_dir()?;
        let scratch = tempfile::tempdir_in(".")?;
        let socket_root = scratch.path().strip_prefix(&root)?;
        for artifacts in [
            root.join("target/e2e-gate/e2e__test/fleet::s16_in_flight_terminal_blocks_drain_and_disconnect_keeps_fence_replay/1234567890123456789/case/s16-replay"),
            root.join("deep/".repeat(100)).join("資料".repeat(100)),
        ] {
            let primary = control_socket(&artifacts.join("primary"), socket_root);
            let successor = control_socket(&artifacts.join("successor"), socket_root);
            ensure!(primary != successor, "fleet hosts must have distinct sockets");
            ensure!(
                primary.as_os_str().as_encoded_bytes().len() < 108,
                "fleet control socket exceeds SUN_LEN: {}",
                primary.display()
            );
            let listener = std::os::unix::net::UnixListener::bind(&primary)?;
            let _client = std::os::unix::net::UnixStream::connect(&primary)?;
            drop(listener);
            std::fs::remove_file(primary)?;
        }
        Ok(())
    }

    async fn admin_queries(listener: tokio::net::TcpListener) -> Result<()> {
        let mut moving_partition = true;
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::new();
            let body = loop {
                let mut bytes = [0; 4096];
                let read = stream.read(&mut bytes).await?;
                ensure!(read > 0, "admin request ended before its body");
                request.extend_from_slice(&bytes[..read]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end])?;
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .context("query body length")?
                        .parse()?;
                    if request.len() >= end + 4 + length {
                        break serde_json::from_slice::<Value>(
                            &request[end + 4..end + 4 + length],
                        )?;
                    }
                }
            };
            let query = body["query"].as_str().context("admin SQL query")?;
            ensure!(
                query.contains("'inv-fleet-law'"),
                "query changed invocation: {query}"
            );
            // FIG-5125: after a heal each member can temporarily lose the
            // partition store while its query routing catches up.
            if moving_partition {
                moving_partition = false;
                let response = br#"{"message":"Datafusion error: External error: expecting a partition store"}"#;
                stream.write_all(format!("HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).as_bytes()).await?;
                stream.write_all(response).await?;
                continue;
            }
            let rows = if query.contains("FROM sys_invocation") {
                json!([{"target_service_name":"e2e-fleet-law.LashTurn_g1",
                    "pinned_service_protocol_version":7}])
            } else {
                ensure!(
                    query.contains("FROM sys_journal"),
                    "unexpected query: {query}"
                );
                json!([{"index":0,"entry_type":"Input","name":null,"version":2,
                    "entry_json":"{\"Command\":{\"Input\":{}}}"}])
            };
            let response = serde_json::to_vec(&json!({"rows":rows}))?;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).as_bytes()).await?;
            stream.write_all(&response).await?;
        }
    }

    /// FIG-5041: a recorded fault cannot erase the available members' journals.
    #[tokio::test]
    async fn after_fault_capture_retains_each_available_nodes_journal() -> Result<()> {
        let work = WorkIdentity {
            ingress: "fleet-input".into(),
            run: "fleet-run".into(),
            segment: "inv-fleet-law".into(),
            call: None,
            ordinal: None,
        };
        let mut nodes = Vec::new();
        let mut servers = Vec::new();
        for node in 1..=3 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            nodes.push(NodeReceipt {
                node,
                admin_url: format!("http://{}", listener.local_addr()?),
                ingress_url: String::new(),
                peer_address: String::new(),
                data_directory: String::new(),
                incarnation: 1,
            });
            servers.push(tokio::spawn(admin_queries(listener)));
        }
        let mut evidence = Evidence::empty("fleet".into());
        evidence.faults.push(FaultReceipt {
            fault: Fault::PartitionLink { from: 1, to: 2 },
            proof: BarrierProof {
                barrier: Barrier {
                    work: work.clone(),
                    kind: BarrierKind::TransportConnected,
                },
                artifact: "partition-1-2.json".into(),
                journal_index: None,
            },
            target_incarnation: 1,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        capture_journals(
            &mut evidence,
            &nodes,
            "e2e-fleet-law",
            &work,
            Some(1),
            deadline,
        )
        .await?;
        ensure!(
            evidence.journals.len() == 2,
            "post-fault receipt lacks majority journal evidence"
        );
        ensure!(
            evidence.faults.len() == 1,
            "journal capture erased the fault"
        );
        for (fact, node) in evidence.journals.iter().zip(&nodes[1..]) {
            ensure!(fact.admin_url == node.admin_url && fact.protocol == 7);
            ensure!(fact.work == work && fact.invocation == work.segment);
            ensure!(fact.index == 0 && fact.value == json!({"Command":{"Input":{}}}));
        }
        evidence.journals.clear();
        capture_journals(
            &mut evidence,
            &nodes,
            "e2e-fleet-law",
            &work,
            None,
            deadline,
        )
        .await?;
        ensure!(
            evidence.journals.len() == 3,
            "healed receipt lacks a member's journal evidence"
        );
        for (fact, node) in evidence.journals.iter().zip(&nodes) {
            ensure!(fact.admin_url == node.admin_url && fact.protocol == 7);
            ensure!(fact.work == work && fact.invocation == work.segment);
        }
        for server in servers {
            server.abort();
        }
        Ok(())
    }
}
