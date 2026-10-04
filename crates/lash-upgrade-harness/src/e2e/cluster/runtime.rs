//! Local pinned processes, durable directories and observed cluster convergence.
use super::links::{LinkState, PeerLinks};
use super::{ClusterControl, ClusterReceipt, LeaderReceipt, NodeReceipt};
use crate::e2e::{
    Step,
    case::{ArtifactIdentity, CaseLease},
    control::{BarrierProof, CleanupReceipt, Fault, FaultReceipt, ProcessReceipt},
};
use crate::harness::ServingNode;
use crate::restate_view::RestateView;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

struct Node {
    receipt: NodeReceipt,
    process: Option<ServingNode>,
    config: PathBuf,
    log: PathBuf,
}
pub struct LocalCluster {
    pub port_base: u16,
    nodes: Vec<Node>,
    links: PeerLinks,
    binary: Option<ArtifactIdentity>,
    deadline: Instant,
    provisioning: serde_json::Value,
    namespace: String,
    server_version: String,
}
impl LocalCluster {
    pub fn new(port_base: u16, deadline: Instant) -> Self {
        Self {
            port_base,
            nodes: Vec::new(),
            links: Default::default(),
            binary: None,
            deadline,
            provisioning: serde_json::Value::Null,
            namespace: String::new(),
            server_version: String::new(),
        }
    }
    pub fn nodes(&self) -> Vec<NodeReceipt> {
        self.nodes.iter().map(|node| node.receipt.clone()).collect()
    }
    pub fn link_state(&self) -> Result<LinkState> {
        self.links.snapshot()
    }
    fn node_mut(&mut self, id: u32) -> Result<&mut Node> {
        self.nodes
            .iter_mut()
            .find(|node| node.receipt.node == id)
            .context("unknown Restate node")
    }
    fn start(&mut self, id: u32) -> Result<ProcessReceipt> {
        let binary = self
            .binary
            .as_ref()
            .context("cluster binary was not materialized")?
            .path
            .clone();
        let node = self.node_mut(id)?;
        ensure!(node.process.is_none(), "node already running");
        let mut command = Command::new(binary);
        command
            .args(["--no-logo", "--config-file"])
            .arg(&node.config)
            .env("RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7", "true");
        // Inherited Restate settings could split this private cluster or point at developer services.
        for (key, _) in std::env::vars() {
            if key.starts_with("RESTATE_")
                || [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                ]
                .contains(&key.as_str())
            {
                command.env_remove(key);
            }
        }
        command.env("RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7", "true");
        let process = ServingNode::spawn(&mut command, &node.log)?;
        let pid = process.pid()?;
        node.receipt.incarnation += 1;
        let receipt = ProcessReceipt {
            role: format!("restate-{id}"),
            pid,
            incarnation: node.receipt.incarnation,
            log: node.log.display().to_string(),
        };
        node.process = Some(process);
        self.links.register(id, pid)?;
        Ok(receipt)
    }
    async fn ready(&mut self) -> Result<()> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        loop {
            let mut ready = true;
            for node in &mut self.nodes {
                node.process
                    .as_mut()
                    .context("node is stopped")?
                    .assert_running()?;
                for url in [
                    format!("{}/health", node.receipt.admin_url),
                    format!("{}/restate/health", node.receipt.ingress_url),
                ] {
                    ready &= http
                        .get(url)
                        .send()
                        .await
                        .is_ok_and(|response| response.status().is_success());
                }
            }
            if ready {
                return Ok(());
            }
            ensure!(
                Instant::now() < self.deadline,
                "cluster health never became ready"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    fn control_peer(&self, node: &NodeReceipt) -> Result<String> {
        let offset = u16::try_from((node.node - 1) * 4)?;
        Ok(format!("http://127.0.0.1:{}", self.port_base + offset + 2))
    }
    async fn state(&self, node: &NodeReceipt) -> Result<super::metadata::State> {
        let response: super::metadata::StateResponse = super::metadata::unary(
            &self.control_peer(node)?,
            "restate.cluster_ctrl.ClusterCtrlSvc/GetClusterState",
        )
        .await?;
        response.state.context("control API omitted cluster state")
    }
    fn observed_leaders(state: &super::metadata::State) -> Result<Vec<LeaderReceipt>> {
        let mut leaders = std::collections::BTreeMap::new();
        for (id, node) in &state.nodes {
            if let Some(alive) = &node.alive {
                for (partition, status) in &alive.partitions {
                    if status.effective_mode == 1 && status.detailed_mode == 4 {
                        let epoch = status.epoch.as_ref().context("leader omitted epoch")?.value;
                        ensure!(
                            epoch > 0
                                && status
                                    .leader
                                    .as_ref()
                                    .is_some_and(|leader| leader.id == *id),
                            "leader identity disagrees with its partition status"
                        );
                        ensure!(
                            leaders
                                .insert(
                                    *partition,
                                    LeaderReceipt {
                                        partition: *partition,
                                        node: *id,
                                        epoch
                                    }
                                )
                                .is_none(),
                            "multiple active leaders reported"
                        );
                    }
                }
            }
        }
        Ok(leaders.into_values().collect())
    }
    async fn leader_rows(&self, node: &NodeReceipt) -> Result<Vec<LeaderReceipt>> {
        Self::observed_leaders(&self.state(node).await?)
    }
    /// Provisioning has one observed partition. Confirm the real invocation
    /// exists before assigning its key to that sole partition.
    pub async fn leader_for_invocation(&self, invocation: &str) -> Result<LeaderReceipt> {
        let node = self
            .nodes
            .iter()
            .find(|node| node.process.is_some())
            .context("cluster has no running node")?;
        let view = RestateView::new(&node.receipt.admin_url, "")?;
        let literal = invocation.replace('\'', "''");
        #[derive(Deserialize)]
        struct Row {
            id: String,
        }
        let rows: Vec<Row> = view
            .query(&format!(
                "SELECT id FROM sys_invocation WHERE id = '{literal}'"
            ))
            .await?;
        ensure!(
            rows.len() == 1 && rows[0].id == invocation,
            "invocation is absent"
        );
        let configuration: super::metadata::ConfigurationResponse = super::metadata::unary(
            &self.control_peer(&node.receipt)?,
            "restate.cluster_ctrl.ClusterCtrlSvc/GetClusterConfiguration",
        )
        .await?;
        ensure!(
            configuration
                .configuration
                .is_some_and(|config| config.partitions == 1),
            "invocation mapping requires the observed single-partition table"
        );
        let leaders = self.leader_rows(&node.receipt).await?;
        ensure!(leaders.len() == 1, "single partition has no unique leader");
        Ok(leaders[0].clone())
    }
    pub async fn await_leader_change(&self, prior: &LeaderReceipt) -> Result<LeaderReceipt> {
        loop {
            for node in &self.nodes {
                if node.process.is_none() {
                    continue;
                }
                if let Ok(leaders) = self.leader_rows(&node.receipt).await
                    && let Some(leader) = leaders.into_iter().find(|leader| {
                        leader.partition == prior.partition
                            && leader.node != prior.node
                            && leader.epoch > prior.epoch
                    })
                {
                    return Ok(leader);
                }
            }
            ensure!(
                Instant::now() < self.deadline,
                "active partition did not elect a successor leader"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    pub async fn partition_receipt(&self, from: u32, to: u32) -> Result<usize> {
        ensure!(
            from != to
                && self.nodes.iter().any(|node| node.receipt.node == from)
                && self.nodes.iter().any(|node| node.receipt.node == to),
            "invalid directed link"
        );
        self.links.partition(from, to, self.deadline).await
    }
    async fn converge_inner(&mut self) -> Result<ClusterReceipt> {
        self.ready().await?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        let mut last_error = String::new();
        loop {
            let observed: Result<(Vec<LeaderReceipt>,serde_json::Value,Vec<super::metadata::NodeId>)> = async {
                let mut expected = None;
                let mut views = Vec::new();
                let mut scanner_peers = Vec::new();
                let mut node_version = None;
                for node in &self.nodes {
                    let peer = self.control_peer(&node.receipt)?;
                    let ident: super::metadata::Ident = super::metadata::unary(&peer,"restate.node_ctl_svc.NodeCtlSvc/GetIdent").await?;
                    ensure!(ident.status==1 && ident.node_id.as_ref().is_some_and(|id| id.id==node.receipt.node),"owned node identity is not ready");
                    ensure!(ident.cluster_name==self.namespace(),"node joined another cluster");
                    scanner_peers.push(ident.node_id.clone().context("ready scanner peer lacks identity")?);
                    ensure!(ident.advertised_addresses.iter().any(|address| reqwest::Url::parse(&address.address).ok()==reqwest::Url::parse(&node.receipt.peer_address).ok()),"advertised peer does not use owned link proxy: {:?}",ident.advertised_addresses);
                    if let Some(version) = node_version { ensure!(version==ident.nodes_config_version,"membership versions disagree"); } else { node_version=Some(ident.nodes_config_version); }
                    let response: super::metadata::ConfigurationResponse = super::metadata::unary(&peer,"restate.cluster_ctrl.ClusterCtrlSvc/GetClusterConfiguration").await?;
                    let config = response.configuration.context("cluster configuration is absent")?;
                    std::fs::write(node.config.with_file_name("metadata-observation.json"),serde_json::to_vec_pretty(&json!({"identity":ident,"configuration":config}))?)?;
                    let replication = if self.nodes.len()==3 { "{node: 2}" } else { "{node: 1}" };
                    ensure!(config.partitions==1 && config.replication.as_ref().is_some_and(|value| value.property==replication),"actual partition replication does not match requested cluster");
                    ensure!(config.bifrost.as_ref().is_some_and(|value| value.provider=="replicated" && value.replication.as_ref().is_some_and(|value| value.property==replication)),"actual log replication does not match requested cluster");
                    let state = self.state(&node.receipt).await?;
                    ensure!(state.nodes.len()==self.nodes.len() && self.nodes.iter().all(|owned| state.nodes.get(&owned.receipt.node).is_some_and(|node| node.alive.is_some())),"not all owned nodes are alive in metadata");
                    let leaders = Self::observed_leaders(&state)?;
                    ensure!(leaders.len()==1,"active partition has no unique observed leader");
                    if let Some(prior) = &expected { ensure!(*prior==leaders,"leader views disagree"); } else { expected=Some(leaders); }
                    let health: serde_json::Value = http.get(format!("{}/cluster-health",node.receipt.admin_url)).send().await?.error_for_status()?.json().await?;
                    ensure!(health.pointer("/metadata_cluster_health/members").and_then(serde_json::Value::as_array).is_some_and(|members| members.len()==self.nodes.len()),"metadata quorum has not joined every node");
                    views.push(json!({"identity":ident,"configuration":config,"state":state,"health":health}));
                }
                Ok((expected.context("leaders absent")?,json!({"node_views":views,"peer_identity":"/proc/net/tcp + /proc/<owned-pid>/fd","server_version":self.server_version}),scanner_peers))
            }.await;
            match observed {
                Ok((leaders, mut provisioning, scanner_peers)) => {
                    // Metadata convergence cannot authorize a scanner while
                    // its querying incarnation is still Dead after healing.
                    let logs: Vec<_> = self
                        .nodes
                        .iter()
                        .map(|node| (node.receipt.node, node.log.clone()))
                        .collect();
                    let mut scanner_readiness = Vec::new();
                    for peer in scanner_peers {
                        scanner_readiness.push(
                            super::scanner::await_readiness(peer, logs.clone(), self.deadline)
                                .await?,
                        );
                    }
                    provisioning["scanner_readiness"] = serde_json::to_value(scanner_readiness)?;
                    self.provisioning = provisioning;
                    return Ok(ClusterReceipt {
                        binary: self.binary.clone().context("cluster binary missing")?,
                        nodes: self.nodes(),
                        leaders,
                        provisioning: self.provisioning.clone(),
                    });
                }
                Err(error) => last_error.clone_from(&error.to_string()),
            }
            ensure!(
                Instant::now() < self.deadline,
                "cluster metadata did not converge: {last_error}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    fn namespace(&self) -> &str {
        self.namespace.as_str()
    }
}
impl ClusterControl for LocalCluster {
    fn boot<'a>(
        &'a mut self,
        binary: &'a ArtifactIdentity,
        nodes: usize,
        lease: &'a mut CaseLease,
    ) -> Step<'a, ClusterReceipt> {
        Box::pin(async move {
            binary.verify()?;
            let version = Command::new(&binary.path).arg("--version").output()?;
            ensure!(
                version.status.success(),
                "Restate binary refused version inspection"
            );
            let version = String::from_utf8(version.stdout)?.trim().to_owned();
            ensure!(
                version.split_whitespace().any(|value| value == "1.7.13"),
                "cluster requires the repository Restate 1.7.13 pin, observed {version}"
            );
            self.server_version = version;
            ensure!(nodes == 1 || nodes == 3, "unsupported cluster size");
            ensure!(self.nodes.is_empty(), "cluster already booted");
            ensure!(
                self.port_base >= 61000 && self.port_base <= 65450,
                "cluster needs a gate-owned 50-port block"
            );
            self.binary = Some(binary.clone());
            self.namespace.clone_from(&lease.namespace);
            let mut reserved = Vec::new();
            for offset in 0..nodes * 4 {
                let port = self
                    .port_base
                    .checked_add(u16::try_from(offset)?)
                    .context("port block overflow")?;
                ensure!(!lease.ports.contains(&port), "port already owned");
                reserved
                    .push(Some(TcpListener::bind(("127.0.0.1", port)).with_context(
                        || format!("gate port {port} is unavailable"),
                    )?));
                lease.ports.push(port);
            }
            let peers: Vec<_> = (0..nodes)
                .map(|n| -> Result<String> {
                    Ok(format!(
                        "http://127.0.0.1:{}",
                        self.port_base + u16::try_from(n * 4 + 3)?
                    ))
                })
                .collect::<Result<_>>()?;
            for n in 0..nodes {
                let id = u32::try_from(n + 1)?;
                let base = self.port_base + u16::try_from(n * 4)?;
                let directory = lease.directory.join(format!("restate-{id}"));
                std::fs::create_dir(&directory)?;
                let config = directory.join("restate.toml");
                let data = directory.join("data");
                let peer = format!("127.0.0.1:{}", base + 2);
                let advertised = peers[n].clone();
                let config_text = format!(
                    r#"node-name = "n{id}"
force-node-id = {id}
cluster-name = "{}"
base-dir = {:?}
listen-mode = "tcp"
bind-ip = "127.0.0.1"
bind-port = {}
advertised-address = "{advertised}"
auto-provision = {}
default-num-partitions = 1
default-replication = {}
rocksdb-total-memory-size = "256MB"
default-thread-pool-size = 2
default-thread-pool-spawn-blocking-size = 2
disable-telemetry = true
log-disable-ansi-codes = true
[metadata-client]
type = "replicated"
addresses = {:?}
[admin]
bind-address = "127.0.0.1:{}"
[admin.query-engine]
memory-size = "64MB"
[ingress]
bind-address = "127.0.0.1:{base}"
[bifrost]
default-provider = "replicated"
"#,
                    lease.namespace,
                    data.to_string_lossy(),
                    base + 2,
                    n == 0,
                    if nodes == 3 { 2 } else { 1 },
                    peers,
                    base + 1
                );
                std::fs::write(&config, config_text)?;
                self.links
                    .listen(
                        reserved[n * 4 + 3]
                            .take()
                            .context("proxy port was not reserved")?,
                        id,
                        peer.parse::<SocketAddr>()?,
                    )
                    .await?;
                self.nodes.push(Node {
                    receipt: NodeReceipt {
                        node: id,
                        ingress_url: format!("http://127.0.0.1:{base}"),
                        admin_url: format!("http://127.0.0.1:{}", base + 1),
                        peer_address: advertised,
                        data_directory: data.display().to_string(),
                        incarnation: 0,
                    },
                    process: None,
                    config,
                    log: directory.join("server.log"),
                });
            }
            for n in 0..nodes {
                for offset in 0..3 {
                    drop(reserved[n * 4 + offset].take());
                }
                lease.processes.push(self.start(u32::try_from(n + 1)?)?);
            }
            self.converge_inner().await
        })
    }
    fn leaders(&mut self) -> Step<'_, Vec<LeaderReceipt>> {
        Box::pin(async move {
            let node = self
                .nodes
                .iter()
                .find(|n| n.process.is_some())
                .context("cluster has no serving node")?;
            self.leader_rows(&node.receipt).await
        })
    }
    fn kill<'a>(&'a mut self, id: u32, proof: &'a BarrierProof) -> Step<'a, FaultReceipt> {
        Box::pin(async move {
            ensure!(
                !proof.artifact.is_empty(),
                "missed fault: barrier has no proof"
            );
            let node = self.node_mut(id)?;
            node.process
                .as_mut()
                .context("node was already stopped")?
                .kill_and_reap()?;
            node.process = None;
            Ok(FaultReceipt {
                fault: Fault::KillRestate { node: id },
                proof: proof.clone(),
                target_incarnation: node.receipt.incarnation,
            })
        })
    }
    fn restart(&mut self, id: u32) -> Step<'_, NodeReceipt> {
        Box::pin(async move {
            self.start(id)?;
            self.ready().await?;
            let node = self.node_mut(id)?.receipt.clone();
            let ident: super::metadata::Ident = super::metadata::unary(
                &self.control_peer(&node)?,
                "restate.node_ctl_svc.NodeCtlSvc/GetIdent",
            )
            .await?;
            let peer = ident
                .node_id
                .context("restarted scanner peer has no identity")?;
            ensure!(peer.id == id, "restarted scanner peer identity differs");
            let readiness = super::scanner::await_readiness(
                peer,
                self.nodes
                    .iter()
                    .map(|node| (node.receipt.node, node.log.clone()))
                    .collect(),
                self.deadline,
            )
            .await?;
            std::fs::write(
                self.node_mut(id)?
                    .config
                    .with_file_name("scanner-readiness.json"),
                serde_json::to_vec_pretty(&readiness)?,
            )?;
            Ok(self.node_mut(id)?.receipt.clone())
        })
    }
    fn partition(&mut self, from: u32, to: u32) -> Step<'_, ()> {
        Box::pin(async move {
            self.partition_receipt(from, to).await?;
            Ok(())
        })
    }
    fn heal(&mut self, from: u32, to: u32) -> Step<'_, ()> {
        Box::pin(async move { self.links.heal(from, to) })
    }
    fn converge(&mut self) -> Step<'_, ClusterReceipt> {
        Box::pin(self.converge_inner())
    }
    fn finish(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            let mut receipts = Vec::new();
            let mut failures = Vec::new();
            if let Err(error) = self.links.finish().await {
                failures.push(error.to_string());
            }
            for node in &mut self.nodes {
                if let Some(mut process) = node.process.take() {
                    match process.kill_and_reap() {
                        Ok(()) => {}
                        Err(error) => failures.push(error.to_string()),
                    }
                }
                receipts.push(CleanupReceipt {
                    resource: format!("restate-{}", node.receipt.node),
                    closed: true,
                    detail: "process reaped; durable data and logs retained".into(),
                });
            }
            receipts.push(CleanupReceipt {
                resource: "peer-proxies".into(),
                closed: failures.is_empty(),
                detail: "owned tasks joined".into(),
            });
            for node in &self.nodes {
                for url in [
                    &node.receipt.ingress_url,
                    &node.receipt.admin_url,
                    &node.receipt.peer_address,
                ] {
                    let address = url.trim_start_matches("http://");
                    ensure!(
                        tokio::net::TcpStream::connect(address).await.is_err(),
                        "owned listener leaked at {address}"
                    );
                }
            }
            ensure!(
                failures.is_empty(),
                "cleanup failures: {}",
                failures.join("; ")
            );
            Ok(receipts)
        })
    }
}
