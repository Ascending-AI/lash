//! Public H1 node transport over an owned prebuilt process. Host kills reap
//! that process; admitted work is rebound from the node's actual store/admin
//! address, never from a label or a synthesized journal.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, ensure};

use crate::e2e::{
    Step,
    case::{ArtifactIdentity, CaseLease},
    control::{BarrierProof, CleanupReceipt, Fault, FaultReceipt, ProcessReceipt, WorkIdentity},
    host::{HostAdapter, HostCommand, HostObservation, HostReady},
};
use crate::harness::ServingNode;
use crate::node::{
    RestateArgs, StoreArgs,
    e2e_host::{ProviderHostCommand, ProviderHostConfig, ProviderHostReady},
};

pub struct ProviderNodeHost {
    config: ProviderHostConfig,
    store: StoreArgs,
    restate: RestateArgs,
    process: Option<ServingNode>,
    ready: Option<ProviderHostReady>,
    incarnation: u32,
    sessions: BTreeMap<String, (String, String, WorkIdentity)>,
    observations: Vec<HostObservation>,
}

impl ProviderNodeHost {
    pub fn new(config: ProviderHostConfig, store: StoreArgs, restate: RestateArgs) -> Result<Self> {
        ensure!(
            config.worker_bind.port() > 0 && config.control_bind.port() > 0,
            "cold restart requires reserved stable listener ports"
        );
        Ok(Self {
            config,
            store,
            restate,
            process: None,
            ready: None,
            incarnation: 0,
            sessions: BTreeMap::new(),
            observations: Vec::new(),
        })
    }

    pub async fn request(&mut self, command: &ProviderHostCommand) -> Result<serde_json::Value> {
        self.process
            .as_mut()
            .ok_or_else(|| anyhow!("provider node is not owned"))?
            .assert_running()?;
        let endpoint = &self
            .ready
            .as_ref()
            .ok_or_else(|| anyhow!("provider node is not ready"))?
            .control;
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(self.config.timeout_ms))
            .build()?
            .post(format!("{endpoint}/command"))
            .json(command)
            .send()
            .await?;
        let status = response.status();
        let value = response.json().await?;
        ensure!(
            status.is_success(),
            "H1 node command failed {status}: {value}"
        );
        Ok(value)
    }

    /// Fault placement requires the exact admitted identity the controller
    /// observed. A body receipt may prove acceptance, never durable X/D/V.
    pub fn kill(&mut self, proof: &BarrierProof) -> Result<FaultReceipt> {
        ensure!(
            self.sessions
                .values()
                .any(|(_, _, work)| work.ingress == proof.barrier.work.ingress
                    && work.run == proof.barrier.work.run
                    && work.segment == proof.barrier.work.segment),
            "kill proof names another admitted owner"
        );
        ensure!(!proof.artifact.is_empty(), "kill lacks barrier provenance");
        if proof.barrier.kind.durable() {
            ensure!(
                proof.journal_index.is_some(),
                "durable kill cut lacks actual journal index"
            );
        }
        self.process
            .as_mut()
            .ok_or_else(|| anyhow!("node already stopped"))?
            .kill_and_reap()?;
        self.process = None;
        self.ready = None;
        Ok(FaultReceipt {
            fault: Fault::KillHost {
                target: "h1-provider-node".into(),
            },
            proof: proof.clone(),
            target_incarnation: self.incarnation,
        })
    }

    pub fn ready(&self) -> Result<&ProviderHostReady> {
        self.ready
            .as_ref()
            .ok_or_else(|| anyhow!("node has no ready receipt"))
    }

    async fn address(&mut self, session: &str, id: &str) -> Result<WorkIdentity> {
        let value = self
            .request(&ProviderHostCommand::Address {
                session: session.into(),
                id: id.into(),
            })
            .await?;
        let work: WorkIdentity = serde_json::from_value(value["work"].clone())?;
        ensure!(
            value["invocation"] == work.segment && value["protocol"] == 7,
            "address lacks actual V7 segment correspondence"
        );
        self.sessions
            .insert(work.run.clone(), (session.into(), id.into(), work.clone()));
        Ok(work)
    }
}

impl HostAdapter for ProviderNodeHost {
    fn boot<'a>(
        &'a mut self,
        artifact: &'a ArtifactIdentity,
        lease: &'a mut CaseLease,
    ) -> Step<'a, HostReady> {
        Box::pin(async move {
            ensure!(
                self.process.is_none(),
                "provider node incarnation is still running"
            );
            artifact.verify()?;
            self.incarnation += 1;
            let configuration = lease
                .directory
                .join(format!("h1-host-{}.json", self.incarnation));
            self.config.ready_file = lease
                .directory
                .join(format!("h1-ready-{}.json", self.incarnation));
            std::fs::write(&configuration, serde_json::to_vec(&self.config)?)?;
            let log: PathBuf = lease
                .directory
                .join(format!("h1-host-{}.log", self.incarnation));
            let mut command = Command::new(&artifact.path);
            command
                .arg("e2e-provider-host")
                .arg("--config")
                .arg(configuration)
                .args(["--store", &self.store.store.to_string(), "--data-dir"])
                .arg(&self.store.data_dir)
                .args([
                    "--ingress-url",
                    &self.restate.ingress_url,
                    "--admin-url",
                    &self.restate.admin_url,
                    "--authority",
                    &lease.authority,
                    "--namespace",
                    &lease.namespace,
                ]);
            let mut process = ServingNode::spawn(&mut command, &log)?;
            let ready = loop {
                process.assert_running()?;
                match std::fs::read(&self.config.ready_file) {
                    Ok(bytes) => break serde_json::from_slice::<ProviderHostReady>(&bytes)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                ensure!(
                    Instant::now() < lease.deadline,
                    "provider node never registered"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            ensure!(
                ready.pid == process.pid()?,
                "ready receipt belongs to another process"
            );
            let receipt = ProcessReceipt {
                role: "h1-provider-node".into(),
                pid: ready.pid,
                incarnation: self.incarnation,
                log: log.display().to_string(),
            };
            lease.processes.push(receipt.clone());
            self.ready = Some(ready.clone());
            self.process = Some(process);
            Ok(HostReady {
                endpoint: ready.control,
                process: receipt,
                protocol: 7,
            })
        })
    }

    fn command<'a>(&'a mut self, command: HostCommand) -> Step<'a, HostObservation> {
        Box::pin(async move {
            let attaching = matches!(&command, HostCommand::Attach { .. });
            let observation = match command {
                HostCommand::Submit {
                    session,
                    idempotency_key,
                    input,
                } => {
                    let text = input
                        .as_str()
                        .ok_or_else(|| anyhow!("H1 input must be text"))?
                        .to_owned();
                    let output = self
                        .request(&ProviderHostCommand::Submit {
                            session: session.clone(),
                            id: idempotency_key.clone(),
                            text,
                        })
                        .await?;
                    let work = self.address(&session, &idempotency_key).await?;
                    ensure!(
                        output["run"] == work.run
                            && output["acceptance"]["input_id"] == work.ingress,
                        "public acceptance differs from actual executor address"
                    );
                    HostObservation { work, output }
                }
                HostCommand::Attach { run } | HostCommand::Cancel { run } => {
                    let (session, id, work) = self
                        .sessions
                        .get(&run)
                        .cloned()
                        .ok_or_else(|| anyhow!("Run was not accepted by this adapter"))?;
                    let request = if attaching {
                        ProviderHostCommand::Attach { session, id }
                    } else {
                        ProviderHostCommand::Cancel { session, id }
                    };
                    HostObservation {
                        work,
                        output: self.request(&request).await?,
                    }
                }
                HostCommand::Operation { session, input } if input == "snapshot" => {
                    let work = self
                        .sessions
                        .values()
                        .find(|(accepted, _, _)| accepted == &session)
                        .map(|(_, _, work)| work.clone())
                        .ok_or_else(|| anyhow!("session has no bound Run"))?;
                    HostObservation {
                        work,
                        output: self
                            .request(&ProviderHostCommand::Snapshot { session })
                            .await?,
                    }
                }
                _ => anyhow::bail!("unsupported H1 host command"),
            };
            self.observations.push(observation.clone());
            Ok(observation)
        })
    }

    fn transcript(&self) -> Result<Vec<HostObservation>> {
        Ok(self.observations.clone())
    }

    fn stop(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            if let Some(process) = self.process.take() {
                process.stop()?;
            }
            self.ready = None;
            Ok(vec![CleanupReceipt {
                resource: "h1-provider-node".into(),
                closed: true,
                detail: "owned process stopped and reaped".into(),
            }])
        })
    }
}
