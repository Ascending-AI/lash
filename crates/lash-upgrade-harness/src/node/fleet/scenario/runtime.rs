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
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;

type SharedCluster = Arc<AsyncMutex<LocalCluster>>;
type SharedProxy = Arc<AsyncMutex<Option<V7Proxy>>>;
type Faults = Arc<Mutex<Vec<FaultReceipt>>>;

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
        Box::pin(async move { self.cluster.lock().await.converge().await })
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
        let socket = tokio::net::TcpStream::connect(
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
        }
        Ok(reply)
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
                .args([
                    "--bind",
                    "127.0.0.1:0",
                    "--control-bind",
                    "127.0.0.1:0",
                    "--ready-file",
                ])
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

pub struct RuntimeFixture {
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
            let landings = [
                ("FIG-4894", "736db467f4"),
                ("FIG-4878", "07a7f2cf8b"),
                ("FIG-4739", "82b99419d3"),
            ];
            for requirement in &selected.requires {
                let (_, tip) = landings
                    .iter()
                    .find(|(ticket, _)| ticket == requirement)
                    .context("unknown fleet prerequisite")?;
                ensure!(
                    Command::new("git")
                        .args(["merge-base", "--is-ancestor", tip, "HEAD"])
                        .status()?
                        .success(),
                    "{requirement} prerequisite {tip} has not landed in candidate"
                );
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
                    ingress: boot.nodes[0].ingress_url.clone(),
                    admin: boot.nodes[0].admin_url.clone(),
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
                let b = self
                    .follower
                    .setup("receiver-register", serde_json::Value::Null)
                    .await?;
                ensure!(
                    a == b,
                    "two fleet hosts admitted different mutation receivers"
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
            for node in nodes
                .into_iter()
                .filter(|node| Some(node.node) != excluded_node)
            {
                let mut reader = RestateEvidenceReader::new(
                    "fleet".into(),
                    RestateView::new(&node.admin_url, &self.namespace)?,
                    7,
                );
                reader.bind(work, work.segment.clone())?;
                evidence
                    .journals
                    .extend(reader.collect(work).await?.journals);
            }
            evidence.faults = self
                .faults
                .lock()
                .map_err(|_| anyhow::anyhow!("fault receipts poisoned"))?
                .clone();
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
            let head:serde_json::Value=sqlx::query_scalar("SELECT json_build_object('shift_epoch',m.shift_epoch,'head_revision',h.head_revision,'head',h.head_json)::jsonb FROM lash_session_meta m JOIN lash_session_head h USING(session_id) WHERE m.session_id=$1").bind(&session).fetch_one(pool).await?;
            evidence.stores.push(head);
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
            self.cluster
                .lock()
                .await
                .await_leader_change(previous)
                .await
        })
    }
    fn snapshot<'a>(&'a mut self, work: &'a WorkIdentity) -> Step<'a, FleetSnapshot> {
        Box::pin(async move {
            FleetSnapshot::read(
                self.pool.as_ref().context("PG pool missing")?,
                self.stores
                    .as_ref()
                    .context("PG stores missing")?
                    .session_store_factory()
                    .as_ref(),
                &lash::SessionId::parse(format!("{}-fleet", self.namespace))?,
                &lash::TurnId::parse(&work.run)?,
            )
            .await
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

pub async fn run(scenario: Scenario) -> Result<()> {
    let name = match scenario {
        Scenario::LeaderLoss => "s14",
        Scenario::MinorityPartition => "s15",
        Scenario::TerminalPublication => "s16",
        Scenario::TerminalRedrive => "s16-sigkill",
    };
    run_named(scenario, name).await
}

/// Compose an existing fleet subcase under the caller's unique case slug.
/// The subcase still owns its services, barriers, receipts and cleanup.
pub async fn run_named(scenario: Scenario, name: &str) -> Result<()> {
    let (mut lease, cluster, binary) = setup(name)?;
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
    };
    let result = execute(
        scenario,
        &mut lease,
        &mut cluster_control,
        &mut control,
        &mut fixture,
    )
    .await;
    let executed = usize::from(!fixture.primary.observations.is_empty());
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
fn setup(name: &str) -> Result<(CaseLease, LocalCluster, ArtifactIdentity)> {
    let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
    std::fs::create_dir_all(&root)?;
    let deadline = Instant::now() + Duration::from_secs(240);
    let lease = CaseLease::new(name, root.join(name), deadline)?;
    let base: u16 = std::env::var("LASH_E2E_PORT_BASE")?.parse()?;
    let binary = artifact(
        "restate-server",
        PathBuf::from(std::env::var("LASH_RESTATE_SERVER_BIN")?),
    )?;
    Ok((lease, LocalCluster::new(base, deadline), binary))
}
