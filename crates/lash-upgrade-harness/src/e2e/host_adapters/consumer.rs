use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};

use super::process::{HostProcess, ready};
use crate::e2e::{
    Step,
    case::{ArtifactIdentity, CaseLease},
    control::{CleanupReceipt, WorkIdentity},
    host::{HostAdapter, HostCommand, HostObservation, HostReady},
};

pub struct ConsumerHost {
    pub ingress_url: String,
    pub admin_url: String,
    pub http_port: u16,
    pub endpoint_port: u16,
    http: reqwest::Client,
    process: Option<HostProcess>,
    base: String,
    subjects: BTreeMap<String, (String, String, bool)>,
    observations: Vec<HostObservation>,
    deadline: Instant,
    environment: BTreeMap<String, String>,
}

impl ConsumerHost {
    pub fn new(
        ingress_url: String,
        admin_url: String,
        http_port: u16,
        endpoint_port: u16,
    ) -> Result<Self> {
        Ok(Self {
            ingress_url,
            admin_url,
            http_port,
            endpoint_port,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?,
            process: None,
            base: format!("http://127.0.0.1:{http_port}"),
            subjects: BTreeMap::new(),
            observations: Vec::new(),
            deadline: Instant::now(),
            environment: BTreeMap::new(),
        })
    }

    /// Host-owned scenario configuration, never a production exporter default.
    pub fn configure(mut self, environment: BTreeMap<String, String>) -> Result<Self> {
        ensure!(
            environment.keys().all(|key| matches!(
                key.as_str(),
                "E2E_CONSUMER_STORE"
                    | "E2E_CONSUMER_OTLP_ENDPOINT"
                    | "E2E_CONSUMER_TRACE"
                    | "E2E_CONSUMER_RUN_EFFECT_BUDGET"
                    | "E2E_CONSUMER_SCENARIO"
            )),
            "unsupported consumer fixture configuration"
        );
        self.environment = environment;
        Ok(self)
    }

    pub async fn control(
        &self,
        method: reqwest::Method,
        path: &str,
        input: Option<Value>,
    ) -> Result<Value> {
        ensure!(
            path.starts_with("/control/"),
            "not a consumer control route"
        );
        self.request(method, path, input).await
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        input: Option<Value>,
    ) -> Result<Value> {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(input) = input {
            request = request.json(&input);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        ensure!(
            status.is_success(),
            "consumer {path}: {status} {}",
            String::from_utf8_lossy(&bytes)
        );
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn body_entered(&self, key: &str) -> Result<Value> {
        loop {
            let receipts = self
                .request(reqwest::Method::GET, "/control/entered", None)
                .await?;
            if let Some(receipt) = receipts
                .as_array()
                .context("body receipts are not an array")?
                .iter()
                .find(|receipt| receipt["key"] == key)
            {
                return Ok(receipt.clone());
            }
            ensure!(
                Instant::now() < self.deadline,
                "body-entered barrier missed: {key}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn release(&self, key: &str) -> Result<()> {
        let receipt = self
            .request(reqwest::Method::POST, "/control/release", Some(json!(key)))
            .await?;
        ensure!(
            receipt["released"] == true,
            "release missed its entered body"
        );
        Ok(())
    }

    pub async fn binding(&self, ingress: &str) -> Result<String> {
        let (session, input, operation) = self
            .subjects
            .get(ingress)
            .context("unknown accepted subject")?;
        if *operation {
            return Ok(input.clone());
        }
        let path = format!("/sessions/{session}/inputs/{input}/binding");
        loop {
            let receipt = self.request(reqwest::Method::GET, &path, None).await?;
            if let Some(run) = receipt["run"].as_str() {
                return Ok(run.into());
            }
            ensure!(
                Instant::now() < self.deadline,
                "input has not been bound to a Run"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub fn shutdown_receipt(&self) -> Result<Value> {
        let process = self
            .process
            .as_ref()
            .context("consumer was never spawned")?;
        let log = std::fs::read_to_string(process.log())?;
        let receipt = log
            .lines()
            .find_map(|line| line.strip_prefix("CONSUMER_SHUTDOWN "))
            .context("consumer has not acknowledged orderly shutdown")?;
        Ok(serde_json::from_str(receipt)?)
    }

    pub async fn body_receipts(&self) -> Result<Value> {
        self.request(reqwest::Method::GET, "/control/entered", None)
            .await
    }

    pub async fn binding_receipt(&self, ingress: &str) -> Result<Value> {
        let (session, input, operation) = self
            .subjects
            .get(ingress)
            .context("unknown accepted subject")?;
        ensure!(!operation, "task binding is already its public run");
        self.request(
            reqwest::Method::GET,
            &format!("/sessions/{session}/inputs/{input}/binding"),
            None,
        )
        .await
    }
}

impl HostAdapter for ConsumerHost {
    fn boot<'a>(
        &'a mut self,
        artifact: &'a ArtifactIdentity,
        lease: &'a mut CaseLease,
    ) -> Step<'a, HostReady> {
        Box::pin(async move {
            ensure!(self.process.is_none(), "consumer already booted");
            self.deadline = lease.deadline;
            let mut environment = BTreeMap::from([
                (
                    "E2E_CONSUMER_ADDR".into(),
                    format!("127.0.0.1:{}", self.http_port),
                ),
                (
                    "E2E_CONSUMER_RESTATE_ADDR".into(),
                    format!("127.0.0.1:{}", self.endpoint_port),
                ),
                (
                    "E2E_CONSUMER_DATA_DIR".into(),
                    lease.directory.join("consumer-data").display().to_string(),
                ),
                ("E2E_CONSUMER_STORE".into(), "memory".into()),
                ("E2E_CONSUMER_NAMESPACE".into(), lease.namespace.clone()),
                ("RESTATE_AUTHORITY_ID".into(), lease.authority.clone()),
                ("RESTATE_INGRESS_URL".into(), self.ingress_url.clone()),
                ("RESTATE_ADMIN_URL".into(), self.admin_url.clone()),
            ]);
            environment.extend(self.environment.clone());
            self.process = Some(
                HostProcess::spawn(
                    artifact,
                    lease,
                    "consumer",
                    environment,
                    vec![self.http_port, self.endpoint_port],
                )
                .await?,
            );
            let process = self
                .process
                .as_mut()
                .context("consumer child disappeared")?;
            ready(
                process,
                &self.http,
                &format!("{}/healthz", self.base),
                "external-consumer",
                lease.deadline,
            )
            .await?;
            let listing: Value = self
                .http
                .get(format!("{}/deployments", self.admin_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let uri = format!("http://127.0.0.1:{}", self.endpoint_port);
            let deployment = listing["deployments"]
                .as_array()
                .context("no deployment listing")?
                .iter()
                .find(|deployment| {
                    deployment["uri"]
                        .as_str()
                        .is_some_and(|registered| registered.trim_end_matches('/') == uri)
                })
                .with_context(|| format!("consumer deployment is not registered: {listing}"))?;
            let deployment: Value = self
                .http
                .get(format!(
                    "{}/deployments/{}",
                    self.admin_url,
                    deployment["id"].as_str().context("deployment has no id")?
                ))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            std::fs::write(
                lease.directory.join("consumer-deployment.json"),
                serde_json::to_vec_pretty(&deployment)?,
            )?;
            let version: Value = self
                .http
                .get(format!("{}/version", self.admin_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(
                version["features"]["protocol_v7"] == true
                    && deployment["max_protocol_version"].as_u64() == Some(7),
                "consumer must negotiate V7: {deployment}; server {version}"
            );
            Ok(HostReady {
                endpoint: self.base.clone(),
                process: process.receipt.clone(),
                protocol: 7,
            })
        })
    }

    fn command<'a>(&'a mut self, command: HostCommand) -> Step<'a, HostObservation> {
        Box::pin(async move {
            let cancelling = matches!(&command, HostCommand::Cancel { .. });
            let (ingress, session, subject, operation, output) = match command {
                HostCommand::Submit {
                    session,
                    idempotency_key,
                    input,
                } => {
                    let output = self.request(reqwest::Method::POST, &format!("/sessions/{session}/inputs"), Some(json!({"id":idempotency_key,"text":input.as_str().context("consumer turn input must be text")?}))).await?;
                    let subject = output["input_id"]
                        .as_str()
                        .context("no durable acceptance input identity")?
                        .to_string();
                    self.subjects
                        .insert(subject.clone(), (session.clone(), subject.clone(), false));
                    (subject.clone(), session, subject, false, output)
                }
                HostCommand::Operation { session, input } => {
                    let output = self
                        .request(
                            reqwest::Method::POST,
                            &format!("/sessions/{session}/tasks"),
                            Some(input),
                        )
                        .await?;
                    let run = output["run"]
                        .as_str()
                        .context("no operation Run identity")?
                        .to_string();
                    self.subjects
                        .insert(run.clone(), (session.clone(), run.clone(), true));
                    (run.clone(), session, run, true, output)
                }
                HostCommand::Attach { run } | HostCommand::Cancel { run } => {
                    let (ingress, (session, subject, operation)) = self
                        .subjects
                        .iter()
                        .find(|(ingress, (_, subject, _))| **ingress == run || *subject == run)
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .context("unknown consumer subject")?;
                    let family = if operation { "runs" } else { "inputs" };
                    let cancel = cancelling;
                    let suffix = if cancel { "/cancel" } else { "" };
                    let output = self
                        .request(
                            if cancel {
                                reqwest::Method::POST
                            } else {
                                reqwest::Method::GET
                            },
                            &format!("/sessions/{session}/{family}/{subject}{suffix}"),
                            None,
                        )
                        .await?;
                    (ingress, session, subject, operation, output)
                }
                _ => bail!(
                    "consumer supports send/follow/cancel/operation; unsupported control must refuse"
                ),
            };
            let _ = session;
            let work = WorkIdentity {
                ingress,
                run: if operation { subject } else { String::new() },
                segment: String::new(),
                call: None,
                ordinal: None,
            };
            let observation = HostObservation { work, output };
            self.observations.push(observation.clone());
            Ok(observation)
        })
    }

    fn transcript(&self) -> Result<Vec<HostObservation>> {
        Ok(self.observations.clone())
    }
    fn stop(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            match self.process.as_mut() {
                Some(process) => {
                    process
                        .stop(
                            Instant::now() + Duration::from_secs(20),
                            Some("CONSUMER_SHUTDOWN "),
                        )
                        .await
                }
                None => Ok(Vec::new()),
            }
        })
    }
}
