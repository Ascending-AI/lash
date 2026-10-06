//! The real workbench HTTP and recoverable-chat observation transport.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use lash::remote::{Negotiated, Negotiation, REMOTE_PROTOCOL};
use serde_json::{Value, json};

use super::process::{HostProcess, ready};
use crate::e2e::{
    Step,
    case::{ArtifactIdentity, CaseLease},
    control::{CleanupReceipt, WorkIdentity},
    host::{HostAdapter, HostCommand, HostObservation, HostReady},
};

mod mcp;
mod store;

/// The PostgreSQL live replay mode (FIG-5101): with
/// `LASH_E2E_LIVE_REPLAY=postgresql`, a workbench whose case names no live
/// replay store runs on the shared PostgreSQL store at
/// `LASH_POSTGRES_DATABASE_URL`, in a schema of its case's namespace, so
/// replicas of one case share it and cases never do.
fn live_replay_mode(environment: &mut BTreeMap<String, String>, namespace: &str) -> Result<()> {
    match std::env::var("LASH_E2E_LIVE_REPLAY").ok().as_deref() {
        None | Some("memory") => return Ok(()),
        Some("postgresql") => {}
        Some(other) => bail!("LASH_E2E_LIVE_REPLAY=`{other}` names no live replay mode"),
    }
    if environment.contains_key("AGENT_WORKBENCH_LIVE_REPLAY_STORE") {
        return Ok(());
    }
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .context("the PostgreSQL live replay mode needs LASH_POSTGRES_DATABASE_URL")?;
    let schema = format!(
        "live_replay_{}",
        &lash_core::stable_hash::sha256_hex(namespace.as_bytes())[..32]
    );
    environment.extend([
        (
            "AGENT_WORKBENCH_LIVE_REPLAY_STORE".to_string(),
            "postgresql".to_string(),
        ),
        (
            "AGENT_WORKBENCH_LIVE_REPLAY_DATABASE_URL".to_string(),
            database_url,
        ),
        (
            "AGENT_WORKBENCH_LIVE_REPLAY_CONFIG".to_string(),
            json!({ "schema": schema }).to_string(),
        ),
    ]);
    Ok(())
}

pub struct WorkbenchHost {
    mcp: Option<HostProcess>,
    mcp_artifact: Option<ArtifactIdentity>,
    mcp_cleanup: Vec<CleanupReceipt>,
    ingress: String,
    admin: String,
    http_port: u16,
    endpoint_port: u16,
    environment: BTreeMap<String, String>,
    http: reqwest::Client,
    process: Option<HostProcess>,
    directory: Option<PathBuf>,
    artifact: Option<ArtifactIdentity>,
    lease: Option<CaseLease>,
    cleanup: Vec<CleanupReceipt>,
    sessions: BTreeMap<String, String>,
    subjects: BTreeMap<String, String>,
    operations: BTreeSet<String>,
    transcript: Vec<HostObservation>,
}

impl WorkbenchHost {
    pub fn new(ingress: String, admin: String, http_port: u16, endpoint_port: u16) -> Result<Self> {
        Ok(Self {
            mcp: None,
            mcp_artifact: None,
            mcp_cleanup: Vec::new(),
            ingress,
            admin,
            http_port,
            endpoint_port,
            environment: BTreeMap::new(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(90))
                .build()?,
            process: None,
            directory: None,
            artifact: None,
            lease: None,
            cleanup: Vec::new(),
            sessions: BTreeMap::new(),
            subjects: BTreeMap::new(),
            operations: BTreeSet::new(),
            transcript: Vec::new(),
        })
    }

    pub fn configure(mut self, environment: BTreeMap<String, String>) -> Result<Self> {
        ensure!(
            environment.keys().all(|key| matches!(
                key.as_str(),
                "AGENT_WORKBENCH_MCP_STDIO_PID"
                    | "AGENT_WORKBENCH_MCP_PROVIDER_LOG"
                    | "AGENT_WORKBENCH_MCP_FIXTURE_BIN"
                    | "AGENT_WORKBENCH_SEARCH_MCP_URL"
                    | "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO"
                    | "AGENT_WORKBENCH_PROVIDER_URL"
                    | "AGENT_WORKBENCH_PROTOCOL"
                    | "AGENT_WORKBENCH_TOOL_FIXTURE"
                    | "AGENT_WORKBENCH_DATA_DIR"
                    | "AGENT_WORKBENCH_DATABASE_URL"
                    | "AGENT_WORKBENCH_LIVE_REPLAY_STORE"
                    | "AGENT_WORKBENCH_LIVE_REPLAY_CONFIG"
                    | "AGENT_WORKBENCH_LIVE_REPLAY_DATABASE_URL"
                    | "LASH_HOST_SHUTDOWN_MARKER"
                    | "AGENT_WORKBENCH_RESTATE_ADVERTISE_URL"
                    | "OPENROUTER_API_KEY"
                    | "OPENROUTER_MODEL"
                    | "OPENROUTER_MODEL_VARIANT"
                    | "AGENT_WORKBENCH_OUTPUT_TOKEN_CAP"
            )),
            "unsupported workbench configuration"
        );
        self.environment = environment;
        Ok(self)
    }
    fn advertised_uri(&self) -> String {
        self.environment
            .get("AGENT_WORKBENCH_RESTATE_ADVERTISE_URL")
            .cloned()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", self.endpoint_port))
            .trim_end_matches('/')
            .to_owned()
    }
    /// Start the workbench child. `register` runs the product's deployment
    /// registration; a process restarted in place serves its existing one.
    async fn spawn(&mut self, register: bool) -> Result<HostReady> {
        let artifact = self
            .artifact
            .clone()
            .context("workbench artifact missing")?;
        let mut lease = copy_lease(self.lease.as_ref().context("workbench lease missing")?);
        let mut environment = BTreeMap::from([
            (
                "AGENT_WORKBENCH_ADDR".into(),
                format!("127.0.0.1:{}", self.http_port),
            ),
            (
                "AGENT_WORKBENCH_RESTATE_ADDR".into(),
                format!("127.0.0.1:{}", self.endpoint_port),
            ),
            (
                "AGENT_WORKBENCH_RESTATE_NAMESPACE".into(),
                lease.namespace.clone(),
            ),
            (
                "AGENT_WORKBENCH_DATA_DIR".into(),
                self.data_directory()?.display().to_string(),
            ),
            ("AGENT_WORKBENCH_OPEN".into(), "0".into()),
            ("RESTATE_AUTHORITY_ID".into(), lease.authority.clone()),
            ("RESTATE_INGRESS_URL".into(), self.ingress.clone()),
            ("RESTATE_ADMIN_URL".into(), self.admin.clone()),
        ]);
        environment.extend(self.environment.clone());
        live_replay_mode(&mut environment, &lease.namespace)?;
        self.process = Some(
            HostProcess::spawn(
                &artifact,
                &mut lease,
                "workbench",
                environment.clone(),
                vec![self.http_port, self.endpoint_port],
            )
            .await?,
        );
        let health = format!("{}/healthz", self.base());
        ready(
            self.process.as_mut().context("workbench child missing")?,
            &self.http,
            &health,
            "agent-workbench",
            lease.deadline,
        )
        .await?;
        if register {
            let mut registration = HostProcess::spawn_with_args(
                &artifact,
                &mut lease,
                "workbench-registration",
                environment,
                Vec::new(),
                &["register-deployment".into(), self.advertised_uri()],
            )
            .await?;
            let result = registration.finish(lease.deadline).await;
            let cleanup = registration
                .stop(Instant::now() + Duration::from_secs(10), None)
                .await;
            if let Ok(receipts) = &cleanup {
                lease.cleanup.extend(receipts.clone());
            }
            result?;
            cleanup?;
        }
        let listing: Value = self
            .http
            .get(format!("{}/deployments", self.admin))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let uri = self.advertised_uri();
        let deployment = listing["deployments"]
            .as_array()
            .context("no registered deployments")?
            .iter()
            .find(|row| {
                row["uri"]
                    .as_str()
                    .is_some_and(|u| u.trim_end_matches('/') == uri)
            })
            .context("workbench endpoint not registered")?;
        let deployment: Value = self
            .http
            .get(format!(
                "{}/deployments/{}",
                self.admin,
                deployment["id"].as_str().context("deployment id missing")?
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            deployment["max_protocol_version"] == 7,
            "workbench did not negotiate V7"
        );
        std::fs::write(
            lease.directory.join("workbench-deployment.json"),
            serde_json::to_vec_pretty(&deployment)?,
        )?;
        let ready = HostReady {
            endpoint: self.base(),
            process: self
                .process
                .as_ref()
                .context("workbench child missing")?
                .receipt
                .clone(),
            protocol: 7,
        };
        self.lease = Some(lease);
        Ok(ready)
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

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.http_port)
    }
    pub fn data_directory(&self) -> Result<PathBuf> {
        if let Some(directory) = self.environment.get("AGENT_WORKBENCH_DATA_DIR") {
            return Ok(PathBuf::from(directory));
        }
        Ok(self
            .directory
            .as_ref()
            .context("workbench not booted")?
            .join("workbench-data"))
    }
    pub fn trace_path(&self) -> Result<PathBuf> {
        Ok(self.data_directory()?.join("trace.jsonl"))
    }
    pub fn session_id(&self, alias: &str) -> Result<&str> {
        Ok(self
            .sessions
            .get(alias)
            .context("unknown workbench session alias")?)
    }

    pub async fn control(
        &self,
        method: reqwest::Method,
        path: &str,
        input: Option<Value>,
    ) -> Result<Value> {
        ensure!(path.starts_with("/api/"), "not a workbench API path");
        let mut request = self.http.request(method, format!("{}{path}", self.base()));
        if let Some(input) = input {
            request = request.json(&input);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        ensure!(
            status.is_success(),
            "workbench {path}: {status} {}",
            String::from_utf8_lossy(&bytes)
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
    /// Reads synced callback deliveries, never synthesizing body identities.
    async fn await_bodies(&self, input: &Value) -> Result<Value> {
        let labels = input["labels"]
            .as_array()
            .context("labels must be an array")?;
        let run = input["run"].as_str().context("actual run is required")?;
        loop {
            let mut bodies = Vec::new();
            for entry in std::fs::read_dir(
                self.directory
                    .as_ref()
                    .context("workbench directory missing")?
                    .join("barriers"),
            )? {
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
    async fn session(&mut self, alias: &str) -> Result<String> {
        if let Some(id) = self.sessions.get(alias) {
            return Ok(id.clone());
        }
        let created = self
            .control(
                reqwest::Method::POST,
                "/api/sessions",
                Some(json!({"name":alias})),
            )
            .await?;
        let id = created["session_id"]
            .as_str()
            .context("new session has no actual id")?
            .to_owned();
        self.sessions.insert(alias.into(), id.clone());
        Ok(id)
    }
    pub async fn snapshot(&self, session: &str) -> Result<Value> {
        let query = serde_url_query(session, None);
        self.control(reqwest::Method::GET, &format!("/api/state?{query}"), None)
            .await
    }
    /// The caller owns the stream. Preserve headers/chunks and drop it after
    /// collecting the scenario's actual terminal replay; no local projection.
    pub async fn observations(&self, session: &str, cursor: &str) -> Result<reqwest::Response> {
        ensure!(
            !session.is_empty() && !cursor.is_empty(),
            "late observation requires actual session and pre-input cursor"
        );
        let response = self
            .http
            .get(format!(
                "{}/api/observations?{}",
                self.base(),
                serde_url_query(session, Some(cursor))
            ))
            .header(
                "x-lash-protocol-hello",
                serde_json::to_string(&Negotiation::Hello {
                    supported: REMOTE_PROTOCOL,
                })?,
            )
            .send()
            .await?
            .error_for_status()?;
        let accept: Negotiation = serde_json::from_str(
            response
                .headers()
                .get("x-lash-protocol-accept")
                .context("workbench omitted protocol Accept")?
                .to_str()?,
        )?;
        Negotiated::from_accept(REMOTE_PROTOCOL, &accept)?;
        Ok(response)
    }
}

fn copy_lease(lease: &CaseLease) -> CaseLease {
    CaseLease {
        gate_id: lease.gate_id.clone(),
        namespace: lease.namespace.clone(),
        authority: lease.authority.clone(),
        directory: lease.directory.clone(),
        postgres_url: lease.postgres_url.clone(),
        ports: lease.ports.clone(),
        deadline: lease.deadline,
        processes: lease.processes.clone(),
        cleanup: lease.cleanup.clone(),
    }
}

// reqwest uses the URL crate's serializer, preserving opaque cursor bytes.
#[expect(
    clippy::expect_used,
    reason = "the fixed local URL is valid before query serialization"
)]
fn serde_url_query(session: &str, cursor: Option<&str>) -> String {
    let mut url = reqwest::Url::parse("http://localhost/").expect("literal URL");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("session_id", session);
        if let Some(cursor) = cursor {
            query.append_pair("cursor", cursor);
        }
    }
    url.query().unwrap_or_default().to_owned()
}

impl HostAdapter for WorkbenchHost {
    fn boot<'a>(
        &'a mut self,
        artifact: &'a ArtifactIdentity,
        lease: &'a mut CaseLease,
    ) -> Step<'a, HostReady> {
        Box::pin(async move {
            ensure!(self.process.is_none(), "workbench already booted");
            self.directory = Some(lease.directory.clone());
            self.artifact = Some(artifact.clone());
            self.lease = Some(copy_lease(lease));
            let ready = self.spawn(true).await?;
            let retained = self.lease.as_ref().context("workbench lease missing")?;
            lease.processes = retained.processes.clone();
            lease.cleanup = retained.cleanup.clone();
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
                    idempotency_key: _,
                    input,
                } => {
                    let session = self.session(&session).await?;
                    let input = if input.is_string() {
                        json!({"text":input})
                    } else {
                        input
                    };
                    let accepted = self
                        .control(
                            reqwest::Method::POST,
                            &format!("/api/turn?{}", serde_url_query(&session, None)),
                            Some(input),
                        )
                        .await?;
                    ensure!(accepted["accepted"] == true, "workbench refused input");
                    work.run = accepted["turn_id"].as_str().unwrap_or_default().into();
                    work.ingress = accepted["queued_input"]["input_id"]
                        .as_str()
                        .unwrap_or_default()
                        .into();
                    if work.ingress.is_empty() && !work.run.is_empty() {
                        let records = self.trace_records()?;
                        let accepted = records
                            .iter()
                            .find(|row| {
                                row["record"]["name"] == "agent_workbench.turn.accepted"
                                    && row["record"]["payload"]["turn_id"] == work.run
                            })
                            .context("workbench omitted actual accepted input trace")?;
                        work.ingress = accepted["record"]["payload"]["input_id"]
                            .as_str()
                            .context("accepted input trace has no input ID")?
                            .into();
                    }
                    if !work.run.is_empty() {
                        self.subjects.insert(work.run.clone(), session.clone());
                    }
                    if !work.ingress.is_empty() {
                        self.subjects.insert(work.ingress.clone(), session.clone());
                    }
                    json!({"session_id":session,"receipt":accepted})
                }
                HostCommand::Operation { session, input } => {
                    let session = self.session(&session).await?;
                    let receipt = self
                        .control(
                            reqwest::Method::POST,
                            &format!("/api/e2e/sessions/{session}/operations"),
                            Some(input),
                        )
                        .await?;
                    work.run = receipt["run"]
                        .as_str()
                        .context("workbench operation returned no Run ID")?
                        .to_owned();
                    self.subjects.insert(work.run.clone(), session.clone());
                    self.operations.insert(work.run.clone());
                    json!({"session_id":session,"receipt":receipt})
                }
                HostCommand::Cancel { run } => {
                    if self.operations.contains(&run) {
                        bail!("workbench operations have no cancel route");
                    }
                    let session = self
                        .subjects
                        .get(&run)
                        .context("unknown workbench run")?
                        .clone();
                    work = self
                        .transcript
                        .iter()
                        .find(|o| o.work.run == run)
                        .context("unknown accepted run")?
                        .work
                        .clone();
                    self.control(
                        reqwest::Method::POST,
                        &format!(
                            "/api/turn/cancel?{}&mode=abort",
                            serde_url_query(&session, None)
                        ),
                        None,
                    )
                    .await?
                }
                HostCommand::Attach { run } => {
                    let session = self
                        .subjects
                        .get(&run)
                        .context("unknown workbench subject")?
                        .clone();
                    if self.operations.contains(&run) {
                        work.run = run.clone();
                        self.control(
                            reqwest::Method::GET,
                            &format!("/api/e2e/sessions/{session}/operations/{run}"),
                            None,
                        )
                        .await?
                    } else {
                        work = self
                            .transcript
                            .iter()
                            .find(|o| o.work.run == run)
                            .context("unknown accepted run")?
                            .work
                            .clone();
                        if self
                            .environment
                            .contains_key("AGENT_WORKBENCH_TOOL_FIXTURE")
                        {
                            json!({"outcome":self.control(reqwest::Method::GET, &format!("/api/e2e/sessions/{session}/inputs/{}",work.ingress),None).await?})
                        } else {
                            json!({"snapshot":self.snapshot(&session).await?})
                        }
                    }
                }
                HostCommand::Transfer { .. } => {
                    bail!("physical handover requires a replacing workbench generation")
                }
                HostCommand::Process { action, input } => match action.as_str() {
                    // `kill-host` kills the workbench's whole process group;
                    // `kill-host-process` only the workbench, so what it
                    // spawned outlives it as after a crash of that process.
                    action @ ("kill-host" | "kill-host-process") => {
                        let process = self.process.as_mut().context("no owned workbench")?;
                        let receipt = process.receipt.clone();
                        if action == "kill-host" {
                            process.kill().await?;
                        } else {
                            process.kill_process().await?;
                        }
                        let cleanup = process
                            .stop(Instant::now() + Duration::from_secs(10), None)
                            .await?;
                        self.cleanup.extend(cleanup.clone());
                        self.process = None;
                        json!({"killed":true,"reaped":true,"process":receipt,"cleanup":cleanup})
                    }
                    "restart" => {
                        ensure!(self.process.is_none(), "workbench still running");
                        serde_json::to_value(self.spawn(true).await?)?
                    }
                    // A rolled-back or crashed build's process returns under
                    // its registered deployment; re-registering would make a
                    // draining generation the newest again.
                    "restart-in-place" => {
                        ensure!(self.process.is_none(), "workbench still running");
                        serde_json::to_value(self.spawn(false).await?)?
                    }
                    "await-tool-bodies" => self.await_bodies(&input).await?,
                    "register-receiver" => {
                        let session = input["session_id"]
                            .as_str()
                            .context("actual session required")?;
                        self.control(
                            reqwest::Method::POST,
                            &format!("/api/e2e/receiver/{session}"),
                            None,
                        )
                        .await?
                    }
                    "effects" => {
                        let process = input["process_id"]
                            .as_str()
                            .context("actual process required")?;
                        self.control(
                            reqwest::Method::GET,
                            &format!("/api/e2e/receiver/{process}/receipts"),
                            None,
                        )
                        .await?
                    }
                    "resolve" => {
                        self.control(reqwest::Method::POST, "/api/e2e/completions", Some(input))
                            .await?
                    }
                    "create-session" => {
                        json!({"session_id":self.session(input["session"].as_str().context("session alias required")?).await?})
                    }
                    "snapshot" => {
                        self.snapshot(
                            input["session_id"]
                                .as_str()
                                .context("actual session required")?,
                        )
                        .await?
                    }
                    "waits" => {
                        self.control(
                            reqwest::Method::GET,
                            &format!(
                                "/api/sessions/{}/waits",
                                input["session_id"]
                                    .as_str()
                                    .context("actual session required")?
                            ),
                            None,
                        )
                        .await?
                    }
                    "operation-bodies" => {
                        self.control(reqwest::Method::GET, "/api/e2e/operations/bodies", None)
                            .await?
                    }
                    "release-operation" => {
                        let key = input["key"].as_str().context("release key required")?;
                        self.control(
                            reqwest::Method::POST,
                            &format!("/api/e2e/operations/{key}/release"),
                            None,
                        )
                        .await?
                    }
                    "admission" => {
                        self.control(
                            reqwest::Method::GET,
                            &format!(
                                "/api/e2e/sessions/{}/admission",
                                input["session_id"]
                                    .as_str()
                                    .context("actual session required")?
                            ),
                            None,
                        )
                        .await?
                    }
                    "trace" => {
                        let records: Vec<Value> = std::fs::read_to_string(self.trace_path()?)?
                            .lines()
                            .filter(|line| !line.trim().is_empty())
                            .map(serde_json::from_str)
                            .collect::<std::result::Result<_, _>>()?;
                        json!({"records":records})
                    }
                    _ => bail!("unsupported workbench control {action}"),
                },
                _ => bail!("unsupported workbench command"),
            };
            let observation = HostObservation { work, output };
            self.transcript.push(observation.clone());
            Ok(observation)
        })
    }
    fn transcript(&self) -> Result<Vec<HostObservation>> {
        Ok(self.transcript.clone())
    }
    fn stop(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            self.cleanup.extend(std::mem::take(&mut self.mcp_cleanup));
            let host = match self.process.as_mut() {
                Some(process) => {
                    process
                        .stop(
                            Instant::now() + Duration::from_secs(30),
                            Some("agent-workbench shutdown complete"),
                        )
                        .await
                }
                None => Ok(Vec::new()),
            };
            let peer = match self.mcp.as_mut() {
                Some(process) => {
                    process
                        .stop(Instant::now() + Duration::from_secs(10), None)
                        .await
                }
                None => Ok(Vec::new()),
            };
            self.cleanup.extend(host?);
            self.cleanup.extend(peer?);
            self.process = None;
            self.mcp = None;
            Ok(self.cleanup.clone())
        })
    }
}
