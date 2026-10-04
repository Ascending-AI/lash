//! Controller-owned agent-service process and its public HTTP contract.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use lash::remote::turn_result::RemoteSendOutcome;
use lash::remote::{Envelope, Negotiation, REMOTE_PROTOCOL};
use serde_json::{Value, json};

use super::process::HostProcess;
use crate::e2e::{
    Step,
    case::{ArtifactIdentity, CaseLease},
    control::{CleanupReceipt, WorkIdentity},
    host::{HostAdapter, HostCommand, HostObservation, HostReady},
};

pub struct AgentServiceHost {
    ingress: String,
    admin: String,
    http_port: u16,
    endpoint_port: u16,
    fixture: Value,
    callback_directory: PathBuf,
    environment: BTreeMap<String, String>,
    http: reqwest::Client,
    process: Option<HostProcess>,
    artifact: Option<ArtifactIdentity>,
    lease: Option<CaseLease>,
    chats: BTreeMap<String, String>,
    subjects: BTreeMap<String, (String, String)>,
    observations: Vec<HostObservation>,
    cleanup: Vec<CleanupReceipt>,
}

impl AgentServiceHost {
    /// `fixture` is the example's own config; its callback listener is owned by H0.
    pub fn new(
        ingress: String,
        admin: String,
        http_port: u16,
        endpoint_port: u16,
        fixture: Value,
        callback_directory: PathBuf,
    ) -> Result<Self> {
        Ok(Self {
            ingress,
            admin,
            http_port,
            endpoint_port,
            fixture,
            callback_directory,
            environment: BTreeMap::new(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(90))
                .build()?,
            process: None,
            artifact: None,
            lease: None,
            chats: BTreeMap::new(),
            subjects: BTreeMap::new(),
            observations: Vec::new(),
            cleanup: Vec::new(),
        })
    }

    pub fn configure(mut self, environment: BTreeMap<String, String>) -> Result<Self> {
        ensure!(
            environment.keys().all(|key| matches!(
                key.as_str(),
                "AGENT_SERVICE_PROTOCOL"
                    | "AGENT_SERVICE_PROVIDER_URL"
                    | "AGENT_SERVICE_RESTATE_ADVERTISE_URL"
                    | "OPENROUTER_API_KEY"
                    | "OPENROUTER_MODEL"
                    | "OPENROUTER_MODEL_VARIANT"
            )),
            "unsupported agent-service host configuration"
        );
        self.environment = environment;
        Ok(self)
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.http_port)
    }

    fn hello(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        Ok(self
            .http
            .request(method, format!("{}{path}", self.base()))
            .header(
                "x-lash-protocol-hello",
                serde_json::to_string(&Negotiation::Hello {
                    supported: REMOTE_PROTOCOL,
                })?,
            ))
    }

    pub async fn control(
        &self,
        method: reqwest::Method,
        path: &str,
        input: Option<Value>,
    ) -> Result<Value> {
        ensure!(path.starts_with("/api/"), "not an agent-service API path");
        let mut request = self.hello(method, path)?;
        if let Some(input) = input {
            request = request.json(&input);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        ensure!(
            status.is_success(),
            "agent-service {path}: {status} {}",
            String::from_utf8_lossy(&bytes)
        );
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn trace_path(&self) -> Result<PathBuf> {
        Ok(self
            .lease
            .as_ref()
            .context("agent-service not booted")?
            .directory
            .join("agent-service-trace.jsonl"))
    }

    pub fn trace_records(&self) -> Result<Vec<Value>> {
        std::fs::read_to_string(self.trace_path()?)?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                Ok(json!({"kind":"h2_trace_record", "record":serde_json::from_str::<Value>(line)?}))
            })
            .collect()
    }

    async fn chat(&mut self, alias: &str) -> Result<String> {
        if let Some(id) = self.chats.get(alias) {
            return Ok(id.clone());
        }
        let chat = self
            .control(
                reqwest::Method::POST,
                "/api/chats",
                Some(json!({"title":alias, "model":self.environment.get("OPENROUTER_MODEL")})),
            )
            .await?;
        let id = chat["id"]
            .as_str()
            .context("created chat has no actual id")?
            .to_owned();
        self.chats.insert(alias.into(), id.clone());
        Ok(id)
    }

    pub fn chat_id(&self, alias: &str) -> Result<&str> {
        Ok(self.chats.get(alias).context("unknown chat alias")?)
    }

    async fn spawn(&mut self) -> Result<HostReady> {
        let lease = self.lease.as_mut().context("agent-service has no lease")?;
        let fixture_path = lease.directory.join("agent-service-fixture.json");
        if !self.fixture.is_null() {
            std::fs::write(&fixture_path, serde_json::to_vec_pretty(&self.fixture)?)?;
        }
        let incarnation = 1 + lease
            .processes
            .iter()
            .filter(|p| p.role == "agent-service")
            .count();
        let mut environment = BTreeMap::from([
            (
                "AGENT_SERVICE_ADDR".into(),
                format!("127.0.0.1:{}", self.http_port),
            ),
            (
                "AGENT_SERVICE_RESTATE_ADDR".into(),
                format!("127.0.0.1:{}", self.endpoint_port),
            ),
            (
                "AGENT_SERVICE_DATA_DIR".into(),
                lease
                    .directory
                    .join("agent-service-data")
                    .display()
                    .to_string(),
            ),
            (
                "AGENT_SERVICE_TRACE".into(),
                lease
                    .directory
                    .join("agent-service-trace.jsonl")
                    .display()
                    .to_string(),
            ),
            (
                "AGENT_SERVICE_RESTATE_NAMESPACE".into(),
                lease.namespace.clone(),
            ),
            ("AGENT_SERVICE_INCARNATION".into(), incarnation.to_string()),
            (
                "WORKER_ID".into(),
                format!("{}-agent-service", lease.gate_id),
            ),
            ("RESTATE_AUTHORITY_ID".into(), lease.authority.clone()),
            ("RESTATE_INGRESS_URL".into(), self.ingress.clone()),
            ("RESTATE_ADMIN_URL".into(), self.admin.clone()),
            ("AGENT_SERVICE_PROTOCOL".into(), "standard".into()),
        ]);
        if !self.fixture.is_null() {
            environment.insert(
                "AGENT_SERVICE_TOOL_FIXTURE".into(),
                fixture_path.display().to_string(),
            );
        }
        environment.extend(self.environment.clone());
        self.process = Some(
            HostProcess::spawn(
                self.artifact
                    .as_ref()
                    .context("agent-service artifact missing")?,
                lease,
                "agent-service",
                environment,
                vec![self.http_port, self.endpoint_port],
            )
            .await?,
        );
        let deadline = lease.deadline;
        loop {
            self.process
                .as_mut()
                .context("missing agent-service child")?
                .check_alive()?;
            if let Ok(response) = self
                .http
                .get(format!("http://127.0.0.1:{}/api/settings", self.http_port))
                .send()
                .await
            {
                if response.status().is_success()
                    && response
                        .json::<Value>()
                        .await
                        .is_ok_and(|value| value["default_profile"].is_string())
                {
                    break;
                }
            }
            ensure!(
                Instant::now() < deadline,
                "agent-service readiness missed deadline"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let listing: Value = self
            .http
            .get(format!("{}/deployments", self.admin))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let uri = self
            .environment
            .get("AGENT_SERVICE_RESTATE_ADVERTISE_URL")
            .cloned()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", self.endpoint_port));
        let uri = uri.trim_end_matches('/');
        let deployment = listing["deployments"]
            .as_array()
            .context("no deployments")?
            .iter()
            .find(|d| {
                d["uri"]
                    .as_str()
                    .is_some_and(|u| u.trim_end_matches('/') == uri)
            })
            .context("agent-service deployment not registered")?;
        let detail: Value = self
            .http
            .get(format!(
                "{}/deployments/{}",
                self.admin,
                deployment["id"].as_str().context("deployment id absent")?
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            detail["max_protocol_version"] == 7,
            "agent-service deployment did not negotiate V7: {detail}"
        );
        std::fs::write(
            self.lease
                .as_ref()
                .context("lease missing")?
                .directory
                .join(format!("agent-service-deployment-{incarnation}.json")),
            serde_json::to_vec_pretty(&detail)?,
        )?;
        Ok(HostReady {
            endpoint: self.base(),
            process: self
                .process
                .as_ref()
                .context("child missing")?
                .receipt
                .clone(),
            protocol: 7,
        })
    }

    /// Reads synced callback deliveries, never synthesizing body identities.
    async fn await_bodies(&self, input: &Value) -> Result<Value> {
        let labels = input["labels"]
            .as_array()
            .context("labels must be an array")?;
        let run = input["run"].as_str().context("actual run is required")?;
        loop {
            let mut bodies = Vec::new();
            for entry in std::fs::read_dir(&self.callback_directory)? {
                let entry = entry?;
                if !entry.file_name().to_string_lossy().starts_with("body-") {
                    continue;
                }
                let record: Value = serde_json::from_slice(&std::fs::read(entry.path())?)?;
                if record["delivery"]["logical_run"] == run
                    && labels.contains(&record["delivery"]["label"])
                {
                    bodies.push(record);
                }
            }
            if labels.iter().all(|label| {
                bodies
                    .iter()
                    .any(|body| &body["delivery"]["label"] == label)
            }) {
                return Ok(json!({"bodies":bodies}));
            }
            ensure!(
                Instant::now() < self.lease.as_ref().context("lease missing")?.deadline,
                "body callback labels were not entered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl HostAdapter for AgentServiceHost {
    fn boot<'a>(
        &'a mut self,
        artifact: &'a ArtifactIdentity,
        lease: &'a mut CaseLease,
    ) -> Step<'a, HostReady> {
        Box::pin(async move {
            ensure!(self.process.is_none(), "agent-service already booted");
            std::fs::create_dir_all(&lease.directory)?;
            self.artifact = Some(artifact.clone());
            self.lease = Some(CaseLease {
                gate_id: lease.gate_id.clone(),
                namespace: lease.namespace.clone(),
                authority: lease.authority.clone(),
                directory: lease.directory.clone(),
                postgres_url: None,
                ports: lease.ports.clone(),
                deadline: lease.deadline,
                processes: lease.processes.clone(),
                cleanup: Vec::new(),
            });
            let ready = self.spawn().await?;
            lease.processes.push(ready.process.clone());
            Ok(ready)
        })
    }

    fn command<'a>(&'a mut self, command: HostCommand) -> Step<'a, HostObservation> {
        Box::pin(async move {
            let mut work = WorkIdentity {
                ingress: String::new(),
                run: String::new(),
                segment: String::new(),
                call: None,
                ordinal: None,
            };
            let output = match command {
                HostCommand::Submit {
                    session,
                    idempotency_key,
                    input,
                } => {
                    ensure!(
                        !self.subjects.contains_key(&idempotency_key),
                        "agent-service does not expose caller-selected idempotency keys"
                    );
                    let chat = self.chat(&session).await?;
                    let mut body = if input.is_string() {
                        json!({"text":input})
                    } else {
                        input
                    };
                    if body.get("board").is_none() {
                        body["board"] = json!({"cells":[null,null,null,null,null,null,null,null,null],"turn":"X"});
                    }
                    let response = self
                        .hello(
                            reqwest::Method::POST,
                            &format!("/api/chats/{chat}/messages"),
                        )?
                        .json(&body)
                        .send()
                        .await?;
                    let status = response.status();
                    if !status.is_success() {
                        let bytes = response.bytes().await?;
                        bail!(
                            "agent-service Submit: {status} {}",
                            String::from_utf8_lossy(&bytes)
                        );
                    }
                    work.ingress = response
                        .headers()
                        .get("x-lash-input-id")
                        .context("missing durable input receipt")?
                        .to_str()?
                        .into();
                    work.run = response
                        .headers()
                        .get("x-lash-turn-id")
                        .context("missing actual run receipt")?
                        .to_str()?
                        .into();
                    let accept: Negotiation = serde_json::from_str(
                        response
                            .headers()
                            .get("x-lash-protocol-accept")
                            .context("no protocol Accept")?
                            .to_str()?,
                    )?;
                    lash::remote::Negotiated::from_accept(REMOTE_PROTOCOL, &accept)?;
                    drop(response);
                    self.subjects
                        .insert(work.run.clone(), (chat.clone(), work.ingress.clone()));
                    self.subjects
                        .insert(work.ingress.clone(), (chat.clone(), work.run.clone()));
                    self.subjects
                        .insert(idempotency_key, (chat.clone(), work.ingress.clone()));
                    json!({"chat_id":chat,"input_id":work.ingress,"run":work.run,"accepted":true})
                }
                HostCommand::Attach { run } => {
                    let (chat, _) = self
                        .subjects
                        .get(&run)
                        .context("unknown accepted subject")?;
                    let family = if self
                        .observations
                        .iter()
                        .any(|o| o.work.run == run && !run.is_empty())
                    {
                        "turns"
                    } else {
                        "inputs"
                    };
                    let response = self
                        .hello(
                            reqwest::Method::GET,
                            &format!("/api/chats/{chat}/{family}/{run}"),
                        )?
                        .send()
                        .await?
                        .error_for_status()?;
                    let bytes = response.bytes().await?;
                    let items: Vec<Value> = bytes
                        .split(|b| *b == b'\n')
                        .filter(|line| !line.is_empty())
                        .map(serde_json::from_slice)
                        .collect::<std::result::Result<_, _>>()?;
                    ensure!(
                        !items.iter().any(|item| item["type"] == "error"),
                        "agent-service follow refused: {items:?}"
                    );
                    let outcome = items
                        .iter()
                        .find(|item| item["type"] == "outcome")
                        .context("follow stream has no terminal outcome")?["outcome"]
                        .clone();
                    let decoded = Envelope::<RemoteSendOutcome>::decode_json(
                        &serde_json::to_vec(&outcome)?,
                        REMOTE_PROTOCOL,
                    )?
                    .into_body();
                    decoded.validate()?;
                    work = self
                        .observations
                        .iter()
                        .find(|o| o.work.run == run || o.work.ingress == run)
                        .context("no subject identity")?
                        .work
                        .clone();
                    json!({"outcome":decoded,"stream":items})
                }
                HostCommand::Cancel { run } => {
                    let original = self
                        .observations
                        .iter()
                        .find(|o| o.work.run == run || o.work.ingress == run)
                        .context("unknown accepted subject")?;
                    work = original.work.clone();
                    let chat = self
                        .subjects
                        .get(&work.run)
                        .context("unknown run")?
                        .0
                        .clone();
                    self.control(
                        reqwest::Method::POST,
                        &format!("/api/chats/{chat}/turns/{}/cancel", work.run),
                        Some(json!({})),
                    )
                    .await?
                }
                HostCommand::Transfer { run } => {
                    work = self
                        .observations
                        .iter()
                        .find(|o| o.work.run == run)
                        .context("unknown run")?
                        .work
                        .clone();
                    let chat = self.subjects.get(&run).context("unknown run")?.0.clone();
                    self.control(
                        reqwest::Method::POST,
                        &format!("/api/e2e/chats/{chat}/handover"),
                        None,
                    )
                    .await?
                }
                HostCommand::Process { action, input } => match action.as_str() {
                    "kill-host" => {
                        let process = self.process.as_mut().context("no owned host")?;
                        let receipt = process.receipt.clone();
                        process.kill().await?;
                        let cleanup = process
                            .stop(Instant::now() + Duration::from_secs(10), None)
                            .await?;
                        self.cleanup.extend(cleanup.clone());
                        self.process = None;
                        json!({"killed":true,"reaped":true,"process":receipt,"cleanup":cleanup})
                    }
                    "restart" => {
                        ensure!(self.process.is_none(), "host still running");
                        serde_json::to_value(self.spawn().await?)?
                    }
                    "await-tool-bodies" => self.await_bodies(&input).await?,
                    "create-chat" => {
                        let alias = input["session"]
                            .as_str()
                            .context("session alias required")?;
                        json!({"chat_id":self.chat(alias).await?})
                    }
                    "register-receiver" => {
                        let chat = input["chat_id"]
                            .as_str()
                            .context("actual chat id required")?;
                        self.control(
                            reqwest::Method::POST,
                            &format!("/api/e2e/receiver/{chat}"),
                            None,
                        )
                        .await?
                    }
                    "effects" => {
                        let process = input["process_id"]
                            .as_str()
                            .context("actual process id required")?;
                        self.control(
                            reqwest::Method::GET,
                            &format!("/api/e2e/receiver/{process}/receipts"),
                            None,
                        )
                        .await?
                    }
                    "trace" => json!({"records":self.trace_records()?}),
                    "snapshot" => {
                        let chat = input["chat_id"]
                            .as_str()
                            .context("actual chat id required")?;
                        self.control(
                            reqwest::Method::GET,
                            &format!("/api/chats/{chat}/messages"),
                            None,
                        )
                        .await?
                    }
                    "resolve" => {
                        self.control(reqwest::Method::POST, "/api/e2e/completions", Some(input))
                            .await?
                    }
                    _ => bail!("unsupported agent-service control: {action}"),
                },
                _ => bail!("unsupported agent-service command"),
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
            if let Some(process) = self.process.as_mut() {
                self.cleanup.extend(
                    process
                        .stop(
                            Instant::now() + Duration::from_secs(20),
                            Some("agent-service shutdown complete"),
                        )
                        .await?,
                );
            }
            Ok(self.cleanup.clone())
        })
    }
}
