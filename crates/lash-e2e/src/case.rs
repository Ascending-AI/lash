//! One case: its store, its nodes, its control endpoint and its receipt.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::control::Control;
use crate::evidence::{CaseBody, CaseReceipt, CleanupReceipt, Evidence, NodeEvidence, Verdict};
use crate::node::{Host, Node, NodeOptions};
use crate::peer::Peer;

/// The store a case's nodes run over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    SqliteMemory,
    SqliteFile,
    Postgresql,
}

impl Store {
    fn spelling(self) -> &'static str {
        match self {
            Self::SqliteMemory => "sqlite_memory",
            Self::SqliteFile => "sqlite_file",
            Self::Postgresql => "postgresql",
        }
    }
}

/// The case's leg: `Live`, or `Resume`, which kills a node at the case's
/// cut and resumes its work on another node from committed state alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leg {
    Live,
    Resume,
}

impl Leg {
    fn spelling(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Resume => "resume",
        }
    }
}

/// A running case.
pub struct Case {
    pub name: String,
    pub store: Store,
    pub leg: Leg,
    /// Whether the case's nodes run on the shared PostgreSQL live replay
    /// store (FIG-5101): its test is named `..._live_replay`.
    pub live_replay: bool,
    /// The case's own directory under the runner's artifacts.
    pub dir: PathBuf,
    pub deadline: Instant,
    pub control: Control,
    pub evidence: Evidence,
    database: Option<lash_postgres_store::testing::IsolatedDatabase>,
    nodes: Vec<Node>,
    peers: Vec<Peer>,
    boots: usize,
    /// The SQLite file the case's hosts keep their store in.
    store_file: PathBuf,
}

fn required(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| {
        format!("{name} is required; a case without its runner's setup is not a passing case")
    })
}

impl Case {
    /// Run `body` as the case `name` declared for `store` and `leg`, and
    /// write its receipt whether or not it passed.
    ///
    /// # Errors
    ///
    /// The case failed, its cleanup did not complete, or the runner's
    /// environment names another permutation.
    pub async fn run(
        name: &str,
        store: Store,
        leg: Leg,
        budget: Duration,
        body: impl AsyncFnOnce(&mut Case) -> Result<()>,
    ) -> Result<()> {
        let mut case = Self::open(name, store, leg, budget).await?;
        let outcome = body(&mut case).await;
        case.finish(outcome).await
    }

    async fn open(name: &str, store: Store, leg: Leg, budget: Duration) -> Result<Self> {
        // The runner names the permutation it selected; a test of another
        // permutation is a stale selector and refuses before it boots. A
        // whole-harness run names none.
        for (variable, spelling) in [
            ("LASH_E2E_STORE", store.spelling()),
            ("LASH_E2E_LEG", leg.spelling()),
        ] {
            if let Ok(selected) = std::env::var(variable) {
                ensure!(
                    selected == spelling,
                    "{name} is the {}/{} permutation; the runner selected {variable}={selected}",
                    store.spelling(),
                    leg.spelling()
                );
            }
        }
        let dir = PathBuf::from(required("LASH_E2E_ARTIFACT_DIR")?).join(name);
        std::fs::create_dir_all(dir.join("data"))?;
        let live_replay = name.ends_with("_live_replay");
        if let Ok(selected) = std::env::var("LASH_E2E_LIVE_REPLAY") {
            ensure!(
                (selected == "postgresql") == live_replay,
                "{name} runs the {} live replay store; the runner selected {selected}",
                if live_replay { "postgresql" } else { "memory" }
            );
        }
        // The case's PostgreSQL database holds its store, its live replay
        // tables, or both.
        let database = if store == Store::Postgresql || live_replay {
            Some(
                lash_postgres_store::testing::IsolatedDatabase::create(&required(
                    "LASH_POSTGRES_DATABASE_URL",
                )?)
                .await,
            )
        } else {
            None
        };
        let control = Control::start().await?;
        Ok(Self {
            name: name.to_owned(),
            store,
            leg,
            live_replay,
            dir,
            deadline: Instant::now() + budget,
            control,
            evidence: Evidence {
                scenario: name.to_owned(),
                ..Evidence::default()
            },
            database,
            nodes: Vec::new(),
            peers: Vec::new(),
            boots: 0,
            store_file: PathBuf::new(),
        })
    }

    /// The case's database URL, on PostgreSQL.
    ///
    /// # Errors
    ///
    /// The case runs on SQLite.
    pub fn database_url(&self) -> Result<&str> {
        Ok(self
            .database
            .as_ref()
            .context("the case has no PostgreSQL database")?
            .url())
    }

    /// Write `value` as the case file `name`, answering its path.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn write(&self, name: &str, value: &Value) -> Result<PathBuf> {
        let path = self.dir.join(name);
        std::fs::write(&path, serde_json::to_vec_pretty(value)?)?;
        Ok(path)
    }

    /// Write the scripted fixture a consumer node reads: the case's tag,
    /// the planned `tools` and the provider's `steps`, with the case's
    /// ledgers and control endpoint.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn fixture(&self, tools: Value, steps: Value) -> Result<PathBuf> {
        self.scripted(tools, steps, json!({}))
    }

    /// [`fixture`](Self::fixture) with named `scripts`: a turn whose input
    /// carries `@<name>` is answered by that script.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn scripted(&self, tools: Value, steps: Value, scripts: Value) -> Result<PathBuf> {
        self.write(
            "fixture.json",
            &json!({
                "tag": self.name,
                "tools": tools,
                "steps": steps,
                "scripts": scripts,
                "body_ledger": self.dir.join("bodies.jsonl"),
                "provider_ledger": self.dir.join("provider.jsonl"),
                "control_url": self.control.url(),
                "reducer_ledger": self.dir.join("reducers.jsonl"),
                "receiver": self.dir.join("receiver.json"),
            }),
        )
    }

    /// Write a workbench H2 fixture for `scenario`: its bodies call the
    /// controller on entry, and an entry of a tool in `holds` is held.
    /// `extra` adds the fixture's optional fields.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn h2(&self, scenario: &str, holds: &[&str], extra: Value) -> Result<PathBuf> {
        let mut fixture = json!({
            "scenario": scenario,
            "delivery_ledger": self.dir.join("bodies.jsonl"),
            "provider_ledger": self.dir.join("provider.jsonl"),
            "body_callback_url": self.control.body_url(holds),
        });
        if let (Some(fields), Some(extra)) = (fixture.as_object_mut(), extra.as_object()) {
            fields.extend(extra.clone());
        }
        self.write("h2.json", &fixture)
    }

    /// The body entries the hosts' ledgers recorded, in order.
    ///
    /// # Errors
    ///
    /// A ledger line that is not JSON.
    pub fn bodies(&self) -> Result<Vec<Value>> {
        crate::read_jsonl(&self.dir.join("bodies.jsonl"))
    }

    /// The model calls the hosts' providers answered, in order.
    ///
    /// # Errors
    ///
    /// A ledger line that is not JSON.
    pub fn model_calls(&self) -> Result<Vec<Value>> {
        crate::read_jsonl(&self.dir.join("provider.jsonl"))
    }

    /// Boot a node of `host` named `name`.
    ///
    /// # Errors
    ///
    /// The host binary is missing or the node does not serve.
    pub async fn boot(&mut self, host: Host, name: &str, options: NodeOptions) -> Result<usize> {
        let binary = self.binary(host)?;
        let ledger = self.dir.join(format!("commits-{name}.jsonl"));
        let mut env = vec![("RUST_LOG".to_owned(), "warn".to_owned())];
        if let Ok(worker) = std::env::var("LASH_VM_WORKER") {
            env.push(("LASH_VM_WORKER".to_owned(), worker));
        }
        // Nodes over one SQLite file share its directory, one at a time; a
        // workbench keeps host files beside its store, so workbench nodes
        // over PostgreSQL, which may serve together, each keep their own.
        let data = match (host, self.store) {
            (Host::Workbench, Store::Postgresql) => self.dir.join(format!("data-{name}")),
            _ => self.dir.join("data"),
        };
        self.store_file = data.join(match host {
            Host::Consumer => "lash.db",
            Host::Workbench => "lash-sessions.db",
        });
        match host {
            Host::Consumer => {
                let store = match self.store {
                    Store::SqliteMemory => "memory",
                    Store::SqliteFile => "file",
                    Store::Postgresql => "postgres",
                };
                env.extend([
                    (
                        "E2E_CONSUMER_DATA_DIR".to_owned(),
                        data.display().to_string(),
                    ),
                    ("E2E_CONSUMER_STORE".to_owned(), store.to_owned()),
                    ("E2E_CONSUMER_NODE".to_owned(), name.to_owned()),
                    (
                        "E2E_CONSUMER_COMMIT_LEDGER".to_owned(),
                        ledger.display().to_string(),
                    ),
                    (
                        "E2E_CONSUMER_CUTS".to_owned(),
                        serde_json::to_string(&options.cuts)?,
                    ),
                ]);
                if self.store == Store::Postgresql {
                    let url = match &options.database_url {
                        Some(url) => url.clone(),
                        None => self.database_url()?.to_owned(),
                    };
                    env.push(("E2E_CONSUMER_DATABASE_URL".to_owned(), url));
                }
                if self.live_replay {
                    env.push((
                        "E2E_CONSUMER_LIVE_REPLAY_URL".to_owned(),
                        self.database_url()?.to_owned(),
                    ));
                }
                if let Some(fixture) = &options.fixture {
                    env.push((
                        "E2E_CONSUMER_FIXTURE".to_owned(),
                        fixture.display().to_string(),
                    ));
                }
            }
            Host::Workbench => {
                env.extend([
                    (
                        "AGENT_WORKBENCH_DATA_DIR".to_owned(),
                        data.display().to_string(),
                    ),
                    ("AGENT_WORKBENCH_NODE".to_owned(), name.to_owned()),
                    (
                        "AGENT_WORKBENCH_COMMIT_LEDGER".to_owned(),
                        ledger.display().to_string(),
                    ),
                    (
                        "AGENT_WORKBENCH_COMMIT_CUTS".to_owned(),
                        serde_json::to_string(&options.cuts)?,
                    ),
                    // No case reaches the public search server.
                    (
                        "AGENT_WORKBENCH_SEARCH_MCP_URL".to_owned(),
                        format!("http://127.0.0.1:{}/mcp", crate::node::free_port()?),
                    ),
                ]);
                if self.store == Store::Postgresql {
                    let url = match &options.database_url {
                        Some(url) => url.clone(),
                        None => self.database_url()?.to_owned(),
                    };
                    env.push(("AGENT_WORKBENCH_DATABASE_URL".to_owned(), url));
                }
                if self.live_replay {
                    env.extend([
                        (
                            "AGENT_WORKBENCH_LIVE_REPLAY_STORE".to_owned(),
                            "postgresql".to_owned(),
                        ),
                        (
                            "AGENT_WORKBENCH_LIVE_REPLAY_DATABASE_URL".to_owned(),
                            self.database_url()?.to_owned(),
                        ),
                    ]);
                }
                if let Some(fixture) = &options.fixture {
                    env.push((
                        "AGENT_WORKBENCH_TOOL_FIXTURE".to_owned(),
                        fixture.display().to_string(),
                    ));
                }
            }
        }
        env.extend(options.env);
        self.boots += 1;
        let node = Node::spawn(
            host,
            name,
            &binary,
            env,
            &self.dir,
            self.boots,
            ledger,
            self.deadline,
        )
        .await?;
        if let Some(evidence) = self
            .evidence
            .nodes
            .iter_mut()
            .find(|node| node.node == name)
        {
            evidence.boots.push(node.pid);
        } else {
            self.evidence.nodes.push(NodeEvidence {
                node: name.to_owned(),
                boots: vec![node.pid],
                killed: false,
            });
        }
        self.nodes.push(node);
        Ok(self.nodes.len() - 1)
    }

    /// The runner's `host` binary, checked against the digest the runner
    /// built and recorded as one of the case's artifacts.
    ///
    /// # Errors
    ///
    /// The runner names no binary, or the file differs from its digest.
    pub fn binary(&mut self, host: Host) -> Result<PathBuf> {
        let (binary_var, sha_var) = host.variables();
        let binary = PathBuf::from(required(binary_var)?);
        let sha = required(sha_var)?;
        ensure!(
            digest(&binary)? == sha,
            "{} differs from the binary the runner built",
            binary.display()
        );
        if !self
            .evidence
            .artifacts
            .iter()
            .any(|artifact| artifact["sha256"] == sha)
        {
            self.evidence.artifacts.push(json!({
                "role": host.role(),
                "path": binary,
                "sha256": sha,
                "candidate_sha": required("LASH_E2E_CANDIDATE_SHA")?,
            }));
        }
        Ok(binary)
    }

    /// Start peer `name`: the `host` binary run as `args`, such as the
    /// workbench's MCP fixture server, ready once it listens on `address`.
    ///
    /// # Errors
    ///
    /// The binary is not the runner's, or the peer does not listen by the
    /// deadline.
    pub async fn peer(
        &mut self,
        host: Host,
        name: &str,
        args: &[&str],
        env: Vec<(String, String)>,
        address: Option<&str>,
    ) -> Result<u32> {
        let binary = self.binary(host)?;
        self.boots += 1;
        let peer = Peer::spawn(
            name,
            &binary,
            args,
            env,
            address,
            &self.dir,
            self.boots,
            self.deadline,
        )
        .await?;
        let pid = peer.pid();
        self.peers.push(peer);
        Ok(pid)
    }

    /// SIGKILL the live peer `name`, recording the fault.
    ///
    /// # Errors
    ///
    /// No live peer has that name, or it cannot be killed.
    pub async fn kill_peer(&mut self, name: &str, at: &str) -> Result<()> {
        let peer = self
            .peers
            .iter_mut()
            .rev()
            .find(|peer| peer.name == name && peer.child.is_some())
            .with_context(|| format!("no live peer {name}"))?;
        peer.kill().await?;
        self.evidence.cleanup.push(CleanupReceipt {
            resource: format!("peer {name} process {}", peer.pid),
            closed: true,
            detail: "killed and reaped".to_owned(),
        });
        self.evidence
            .faults
            .push(json!({"fault": "sigkill", "peer": name, "at": at}));
        Ok(())
    }

    /// The boot `index` [`boot`](Self::boot) answered, live or not.
    ///
    /// # Errors
    ///
    /// No boot has that index.
    pub fn boot_at(&self, index: usize) -> Result<&Node> {
        self.nodes
            .get(index)
            .with_context(|| format!("no boot {index}"))
    }

    /// The live boot of node `name`.
    ///
    /// # Errors
    ///
    /// No live boot has that name.
    pub fn node(&self, name: &str) -> Result<&Node> {
        self.nodes
            .iter()
            .rev()
            .find(|node| node.name == name && node.child.is_some())
            .with_context(|| format!("no live node {name}"))
    }

    /// SIGKILL the live boot of `name`, recording the fault.
    ///
    /// # Errors
    ///
    /// No live boot has that name, or it cannot be killed.
    pub async fn kill(&mut self, name: &str, at: &str) -> Result<()> {
        let node = self
            .nodes
            .iter_mut()
            .rev()
            .find(|node| node.name == name && node.child.is_some())
            .with_context(|| format!("no live node {name}"))?;
        node.kill().await?;
        if let Some(evidence) = self
            .evidence
            .nodes
            .iter_mut()
            .find(|node| node.node == name)
        {
            evidence.killed = true;
        }
        self.evidence
            .faults
            .push(json!({"fault": "sigkill", "node": name, "at": at}));
        Ok(())
    }

    /// Stop the live boot of `name` cleanly, recording its cleanup.
    ///
    /// # Errors
    ///
    /// It does not stop by the deadline.
    pub async fn stop(&mut self, name: &str) -> Result<()> {
        let deadline = self.deadline;
        let node = self
            .nodes
            .iter_mut()
            .rev()
            .find(|node| node.name == name && node.child.is_some())
            .with_context(|| format!("no live node {name}"))?;
        let detail = node.stop(deadline).await?;
        self.evidence.cleanup.push(CleanupReceipt {
            resource: format!("node {name} boot {}", node.pid),
            closed: true,
            detail,
        });
        Ok(())
    }

    /// Wait for `probe` to answer, polling until the case's deadline.
    ///
    /// # Errors
    ///
    /// The probe fails, or answers nothing by the deadline.
    pub async fn until<T, F, Fut>(&self, what: &str, mut probe: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Option<T>>>,
    {
        loop {
            if let Some(answer) = probe().await? {
                return Ok(answer);
            }
            if Instant::now() >= self.deadline {
                bail!("{what}: not by the case deadline");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait until node `name`'s ledger shows its commit held at `label`.
    ///
    /// # Errors
    ///
    /// It is not held by the deadline.
    pub async fn held(&self, name: &str, label: &str) -> Result<Value> {
        let node = self.node(name)?;
        let (ledger, boot) = (node.ledger.clone(), node.pid);
        let held = self
            .until(&format!("{name} held at {label}"), || {
                let ledger = ledger.clone();
                async move {
                    Ok(crate::read_jsonl(&ledger)?
                        .into_iter()
                        .find(|line| line["held"] == label && line["boot"] == boot))
                }
            })
            .await?;
        self.record_barrier(json!({"barrier": "commit-cut", "node": name, "held": held}));
        Ok(held)
    }

    /// Record a barrier the case reached.
    pub fn record_barrier(&self, barrier: Value) {
        let path = self.dir.join("barriers.jsonl");
        let mut line = barrier.to_string();
        line.push('\n');
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
    }

    /// The store the case's nodes ran over, opened by the controller to
    /// read back what they committed. On SQLite no node may be running.
    ///
    /// # Errors
    ///
    /// The store does not open, or a node still runs over the SQLite file.
    pub async fn open_store(&self) -> Result<Arc<dyn lash::StoreSet>> {
        Ok(match self.store {
            Store::SqliteFile => {
                ensure!(
                    self.nodes.iter().all(|node| node.child.is_none()),
                    "the SQLite file is read only while no node serves it"
                );
                Arc::new(lash::sqlite::SqliteStoreSet::open(&self.store_file).await?)
            }
            Store::Postgresql => {
                let endpoints = lash::postgres::PostgresEndpoints::from_url(self.database_url()?)?;
                let mut config = lash::postgres::PostgresHostConfig::default();
                config.roles.work.max_connections = 2;
                config.roles.max_store_operations = 2;
                let storage = lash::postgres::PostgresStorage::connect(
                    &endpoints,
                    &config,
                    Default::default(),
                )
                .await?;
                Arc::new(lash::postgres::PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash::persistence::FileAttachmentStore::new(
                        self.dir.join("reader-attachments"),
                    )),
                ))
            }
            Store::SqliteMemory => bail!("a memory store is readable only through its node"),
        })
    }

    async fn finish(mut self, outcome: Result<()>) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        for node in &mut self.nodes {
            if node.child.is_some() {
                let detail = node.stop(deadline).await;
                self.evidence.cleanup.push(CleanupReceipt {
                    resource: format!("node {} boot {}", node.name, node.pid),
                    closed: detail.is_ok(),
                    detail: match detail {
                        Ok(detail) => detail,
                        Err(error) => format!("{error:#}"),
                    },
                });
            } else if node.killed {
                self.evidence.cleanup.push(CleanupReceipt {
                    resource: format!("node {} boot {}", node.name, node.pid),
                    closed: true,
                    detail: "killed and reaped".to_owned(),
                });
            }
        }
        for peer in &mut self.peers {
            if peer.child.is_some() {
                let killed = peer.kill().await;
                self.evidence.cleanup.push(CleanupReceipt {
                    resource: format!("peer {} process {}", peer.name, peer.pid),
                    closed: killed.is_ok(),
                    detail: match killed {
                        Ok(()) => "killed and reaped at teardown".to_owned(),
                        Err(error) => format!("{error:#}"),
                    },
                });
            }
        }
        self.control.stop();
        self.evidence.cleanup.push(CleanupReceipt {
            resource: "control endpoint".to_owned(),
            closed: true,
            detail: self.control.url().to_owned(),
        });
        for node in &self.nodes {
            if !self
                .evidence
                .commits
                .iter()
                .any(|line| line["node"] == node.name.as_str())
            {
                self.evidence
                    .commits
                    .extend(node.commits().unwrap_or_default());
            }
        }
        self.evidence
            .barriers
            .extend(crate::read_jsonl(&self.dir.join("barriers.jsonl")).unwrap_or_default());
        let mut bodies = BTreeMap::new();
        for line in crate::read_jsonl(&self.dir.join("bodies.jsonl")).unwrap_or_default() {
            let identity = format!(
                "{}/{}/{}",
                line["tool"].as_str().unwrap_or_default(),
                line["call_id"].as_str().unwrap_or_default(),
                line["attempt"]
            );
            *bodies.entry(identity).or_insert(0) += 1;
        }
        self.evidence.tripwire.bodies = bodies;
        self.evidence.tripwire.model_calls = crate::read_jsonl(&self.dir.join("provider.jsonl"))
            .unwrap_or_default()
            .len();
        self.evidence.stores.insert(
            0,
            json!({"store": self.store.spelling(), "database": self.database.as_ref().map(|database| database.database_name().to_owned())}),
        );
        if let Some(database) = self.database.take() {
            let name = database.database_name().to_owned();
            drop(database);
            self.evidence.cleanup.push(CleanupReceipt {
                resource: format!("database {name}"),
                closed: true,
                detail: "dropped".to_owned(),
            });
        }
        let verdict = match &outcome {
            Ok(()) => Verdict::Passed,
            Err(error) => Verdict::Failed {
                reason: format!("{error:#}"),
            },
        };
        let receipt = CaseReceipt {
            case: CaseBody {
                evidence: self.evidence,
            },
            verdict,
        };
        std::fs::write(
            self.dir.join("receipt.json"),
            serde_json::to_vec_pretty(&receipt)?,
        )?;
        outcome?;
        let open: Vec<_> = receipt
            .case
            .evidence
            .cleanup
            .iter()
            .filter(|receipt| !receipt.closed)
            .map(|receipt| format!("{}: {}", receipt.resource, receipt.detail))
            .collect();
        ensure!(open.is_empty(), "cleanup incomplete: {open:?}");
        println!("{} executed=1 passed=1", self.name);
        Ok(())
    }
}

fn digest(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(sha2::Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
