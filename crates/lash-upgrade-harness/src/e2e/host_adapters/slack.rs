//! The existing Slack four-layer oracle uses this controller's tracked
//! prebuilt hosts. It never builds or supervises services itself.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;

use super::process::{HostProcess, ready};
use crate::e2e::{
    Step,
    case::{ArtifactIdentity, CaseLease},
    control::{CleanupReceipt, ProcessReceipt, WorkIdentity},
    host::{HostAdapter, HostCommand, HostObservation, HostReady},
};

pub struct SlackHost {
    pub platform: ArtifactIdentity,
    pub mcp: ArtifactIdentity,
    pub stdio_mcp: ArtifactIdentity,
    pub ingress: String,
    pub admin: String,
    pub port: u16,
    pub repo: PathBuf,
    pub python: PathBuf,
    platform_process: Option<HostProcess>,
    mcp_process: Option<HostProcess>,
    bot_process: Option<HostProcess>,
    bot_artifact: Option<ArtifactIdentity>,
    bot_env: BTreeMap<String, String>,
    mcp_env: BTreeMap<String, String>,
    lease: Option<CaseLease>,
    http: reqwest::Client,
    observations: Vec<HostObservation>,
}

impl SlackHost {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        platform: ArtifactIdentity,
        mcp: ArtifactIdentity,
        stdio_mcp: ArtifactIdentity,
        ingress: String,
        admin: String,
        port: u16,
        repo: PathBuf,
        python: PathBuf,
    ) -> Result<Self> {
        Ok(Self {
            platform,
            mcp,
            stdio_mcp,
            ingress,
            admin,
            port,
            repo,
            python,
            platform_process: None,
            mcp_process: None,
            bot_process: None,
            bot_artifact: None,
            bot_env: BTreeMap::new(),
            mcp_env: BTreeMap::new(),
            lease: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .no_proxy()
                .build()?,
            observations: Vec::new(),
        })
    }

    async fn boot_bot(&mut self) -> Result<()> {
        let lease = self.lease.as_mut().context("Slack has no case lease")?;
        let artifact = self
            .bot_artifact
            .as_ref()
            .context("Slack has no bot artifact")?;
        self.bot_process = Some(
            HostProcess::spawn(
                artifact,
                lease,
                "slack-bot",
                self.bot_env.clone(),
                vec![self.port + 1, self.port + 3],
            )
            .await?,
        );
        ready(
            self.bot_process.as_mut().context("missing bot child")?,
            &self.http,
            &format!("http://127.0.0.1:{}/healthz", self.port + 1),
            "slack-clone-bot",
            lease.deadline,
        )
        .await
    }

    async fn boot_mcp(&mut self) -> Result<()> {
        let lease = self.lease.as_mut().context("Slack has no case lease")?;
        self.mcp_process = Some(
            HostProcess::spawn(
                &self.mcp,
                lease,
                "slack-http-mcp",
                self.mcp_env.clone(),
                vec![self.port + 2],
            )
            .await?,
        );
        loop {
            self.mcp_process
                .as_mut()
                .context("missing MCP child")?
                .check_alive()?;
            if tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, self.port + 2))
                .await
                .is_ok()
            {
                break;
            }
            ensure!(
                Instant::now() < lease.deadline,
                "MCP did not bind its real transport"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn action(&mut self, request: Value) -> Result<Value> {
        let root = &self.lease.as_ref().context("Slack has no lease")?.directory;
        match request["action"]
            .as_str()
            .context("controller action is absent")?
        {
            "bot-kill" => {
                ensure!(
                    !request["event"].as_str().unwrap_or_default().is_empty(),
                    "fault lacks active event identity"
                );
                ensure!(
                    root.join("state/provider/kill-provider-entered").is_file(),
                    "bot kill missed the entered provider barrier"
                );
                let process = self.bot_process.as_mut().context("no bot to kill")?;
                let receipt = process.receipt.clone();
                process.kill().await?;
                self.bot_process = None;
                Ok(
                    json!({"reaped":true,"process":receipt,"event":request["event"],"barrier":"provider-entered"}),
                )
            }
            "bot-restart" => {
                ensure!(
                    self.bot_process.is_none(),
                    "restart requires a reaped predecessor"
                );
                self.boot_bot().await?;
                let bot = self.bot_process.as_ref().context("no restarted bot")?;
                Ok(json!({"ready":true,"process":bot.receipt,"log":bot.log()}))
            }
            "mcp-restart" => {
                let marker = root.join("state/mcp-gates/badge-entered");
                let entered: u32 = std::fs::read_to_string(marker)?.trim().parse()?;
                let process = self.mcp_process.as_mut().context("no MCP peer")?;
                ensure!(
                    entered == process.receipt.pid,
                    "MCP barrier belongs to another incarnation"
                );
                let receipt = process.receipt.clone();
                process.kill().await?;
                self.mcp_process = None;
                self.boot_mcp().await?;
                Ok(
                    json!({"reaped":true,"ready":true,"process":receipt,"successor":self.mcp_process.as_ref().map(|process| &process.receipt),"event":request["event"],"barrier":"badge-body-entered"}),
                )
            }
            action => bail!("unknown controller action {action}"),
        }
    }

    pub async fn oracle(&mut self, scenario: &str) -> Result<Value> {
        ensure!(matches!(scenario, "S28" | "S29"), "unknown Slack scenario");
        let lease = self.lease.as_mut().context("Slack has no lease")?;
        let directory = lease.directory.clone();
        let deadline = lease.deadline;
        let bot_log = self
            .bot_process
            .as_ref()
            .context("Slack bot is not serving")?
            .log()
            .to_path_buf();
        let stderr = File::create(directory.join("oracle-stderr.log"))?;
        let mut command = Command::new(&self.python);
        command
            .arg(self.repo.join("scripts/slack-clone-full-host-e2e.py"))
            .args([
                "--repo",
                &self.repo.display().to_string(),
                "--port",
                &self.port.to_string(),
                "--state-dir",
                &directory.join("state").display().to_string(),
                "--artifact-dir",
                &directory.display().to_string(),
                "--bot-log",
                &bot_log.display().to_string(),
                "--scenario",
                scenario,
                "--controller-stdio",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .context("start the preserved browser oracle")?;
        lease.processes.push(ProcessReceipt {
            role: "slack-browser-oracle".into(),
            pid: child.id().context("oracle PID missing")?,
            incarnation: 1,
            log: directory.join("oracle.log").display().to_string(),
        });
        let mut lines = BufReader::new(child.stdout.take().context("oracle stdout")?).lines();
        let mut input = child.stdin.take().context("oracle controller input")?;
        let mut log = File::create(directory.join("oracle.log"))?;
        let execution = async {
            while let Some(line) = tokio::time::timeout(
                deadline.saturating_duration_since(Instant::now()),
                lines.next_line(),
            )
            .await
            .context("browser oracle watchdog")??
            {
                writeln!(log, "{line}")?;
                if let Some(request) = line.strip_prefix("H6_CONTROL ") {
                    let request: Value = serde_json::from_str(request)?;
                    let receipt = match self.action(request.clone()).await {
                        Ok(receipt) => receipt,
                        Err(error) => json!({"error":format!("{error:#}")}),
                    };
                    self.observations.push(HostObservation {
                        work: WorkIdentity {
                            ingress: request["event"].as_str().unwrap_or_default().into(),
                            run: String::new(),
                            segment: String::new(),
                            call: None,
                            ordinal: None,
                        },
                        output: receipt.clone(),
                    });
                    let mut bytes = serde_json::to_vec(&receipt)?;
                    bytes.push(b'\n');
                    input.write_all(&bytes).await?;
                    input.flush().await?;
                }
            }
            let status = child.wait().await?;
            ensure!(
                status.success(),
                "Slack business oracle failed {status}; artifacts {}",
                directory.display()
            );
            let score: Value =
                serde_json::from_slice(&std::fs::read(directory.join("scorecard.json"))?)?;
            ensure!(
                score["selected"] == 1 && score["executed"] == 1 && score["verdict"] == "PASS",
                "Slack receipt has no complete scenario: {score}"
            );
            anyhow::Ok(score)
        }
        .await;
        if execution.is_err() {
            if let Some(pid) = child.id() {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .status()
                    .await;
            }
            let _ = child.wait().await;
        }
        std::fs::write(
            directory.join("controller-faults.json"),
            serde_json::to_vec_pretty(&self.observations)?,
        )?;
        execution
    }
}

impl HostAdapter for SlackHost {
    fn boot<'a>(
        &'a mut self,
        artifact: &'a ArtifactIdentity,
        lease: &'a mut CaseLease,
    ) -> Step<'a, HostReady> {
        Box::pin(async move {
            ensure!(self.lease.is_none(), "Slack case already booted");
            self.lease = Some(CaseLease {
                gate_id: lease.gate_id.clone(),
                namespace: lease.namespace.clone(),
                authority: lease.authority.clone(),
                directory: lease.directory.clone(),
                postgres_url: None,
                ports: lease.ports.clone(),
                deadline: lease.deadline,
                processes: Vec::new(),
                cleanup: Vec::new(),
            });
            let root = lease.directory.join("state");
            let data = root.join(format!("127.0.0.1_{}", self.port));
            std::fs::create_dir_all(root.join("provider"))?;
            let platform_env = BTreeMap::from([
                (
                    "SLACK_CLONE_ADDR".into(),
                    format!("127.0.0.1:{}", self.port),
                ),
                (
                    "SLACK_CLONE_DATA_DIR".into(),
                    data.join("platform").display().to_string(),
                ),
            ]);
            self.platform_process = Some(
                HostProcess::spawn(
                    &self.platform,
                    self.lease.as_mut().context("lease")?,
                    "slack-platform",
                    platform_env,
                    vec![self.port],
                )
                .await?,
            );
            ready(
                self.platform_process.as_mut().context("platform")?,
                &self.http,
                &format!("http://127.0.0.1:{}/healthz", self.port),
                "slack-clone-platform",
                lease.deadline,
            )
            .await?;
            self.mcp_env = BTreeMap::from([
                (
                    "SLACK_CLONE_MCP_HTTP_ADDR".into(),
                    format!("127.0.0.1:{}", self.port + 2),
                ),
                (
                    "SLACK_CLONE_E2E_MCP_GATE_DIR".into(),
                    root.join("mcp-gates").display().to_string(),
                ),
            ]);
            self.boot_mcp().await?;
            self.bot_artifact = Some(artifact.clone());
            self.bot_env = BTreeMap::from([
                (
                    "SLACK_CLONE_BOT_ADDR".into(),
                    format!("127.0.0.1:{}", self.port + 1),
                ),
                (
                    "SLACK_CLONE_BOT_RESTATE_ENDPOINT_ADDR".into(),
                    format!("127.0.0.1:{}", self.port + 3),
                ),
                (
                    "SLACK_CLONE_API_BASE_URL".into(),
                    format!("http://127.0.0.1:{}", self.port),
                ),
                (
                    "SLACK_CLONE_BOT_DATA_DIR".into(),
                    data.join("bot").display().to_string(),
                ),
                (
                    "SLACK_CLONE_BOT_TRACE".into(),
                    data.join("bot/lash/trace.jsonl").display().to_string(),
                ),
                (
                    "SLACK_CLONE_MCP_SERVER".into(),
                    self.stdio_mcp.path.display().to_string(),
                ),
                ("SLACK_CLONE_E2E_PROVIDER".into(), "scripted-v1".into()),
                (
                    "SLACK_CLONE_E2E_PROVIDER_DIR".into(),
                    root.join("provider").display().to_string(),
                ),
                (
                    "SLACK_CLONE_RESTATE_NAMESPACE".into(),
                    lease.namespace.clone(),
                ),
                ("RESTATE_INGRESS_URL".into(), self.ingress.clone()),
                ("RESTATE_ADMIN_URL".into(), self.admin.clone()),
                ("RESTATE_AUTHORITY_ID".into(), lease.authority.clone()),
            ]);
            self.boot_bot().await?;
            let endpoint = format!("http://127.0.0.1:{}", self.port + 3);
            let listing: Value = self
                .http
                .get(format!("{}/deployments", self.admin))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let deployment = listing["deployments"]
                .as_array()
                .context("deployment listing")?
                .iter()
                .find(|deployment| {
                    deployment["uri"]
                        .as_str()
                        .is_some_and(|registered| registered.trim_end_matches('/') == endpoint)
                })
                .context("Slack deployment registration")?;
            let deployment: Value = self
                .http
                .get(format!(
                    "{}/deployments/{}",
                    self.admin,
                    deployment["id"].as_str().context("deployment has no id")?
                ))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            std::fs::write(
                lease.directory.join("slack-deployment.json"),
                serde_json::to_vec_pretty(&deployment)?,
            )?;
            ensure!(
                deployment["max_protocol_version"] == 7,
                "Slack did not negotiate V7: {deployment}"
            );
            let process = self.bot_process.as_ref().context("bot")?.receipt.clone();
            lease
                .processes
                .extend(self.lease.as_ref().context("lease")?.processes.clone());
            Ok(HostReady {
                endpoint: format!("http://127.0.0.1:{}", self.port + 1),
                process,
                protocol: 7,
            })
        })
    }
    fn command<'a>(&'a mut self, command: HostCommand) -> Step<'a, HostObservation> {
        Box::pin(async move {
            let HostCommand::Process { action, input } = command else {
                bail!("Slack controls use its product HTTP/browser oracle");
            };
            let output = self
                .action(json!({"action":action,"event":input["event"]}))
                .await?;
            Ok(HostObservation {
                work: WorkIdentity {
                    ingress: input["event"].as_str().unwrap_or_default().into(),
                    run: String::new(),
                    segment: String::new(),
                    call: None,
                    ordinal: None,
                },
                output,
            })
        })
    }
    fn transcript(&self) -> Result<Vec<HostObservation>> {
        Ok(self.observations.clone())
    }
    fn stop(&mut self) -> Step<'_, Vec<CleanupReceipt>> {
        Box::pin(async move {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut receipts = Vec::new();
            let mut failures = Vec::new();
            for (process, marker) in [
                (
                    &mut self.bot_process,
                    Some("slack-clone-bot plugin shutdown complete"),
                ),
                (&mut self.mcp_process, None),
                (&mut self.platform_process, None),
            ] {
                if let Some(process) = process {
                    match process.stop(deadline, marker).await {
                        Ok(closed) => receipts.extend(closed),
                        Err(error) => failures.push(format!("{error:#}")),
                    }
                }
            }
            let root = &self.lease.as_ref().context("lease")?.directory;
            std::fs::write(
                root.join("slack-cleanup.json"),
                serde_json::to_vec_pretty(
                    &json!({"receipts":receipts,"failures":failures,"processes":self.lease.as_ref().map(|lease| &lease.processes)}),
                )?,
            )?;
            ensure!(failures.is_empty(), "Slack cleanup failed: {failures:?}");
            Ok(receipts)
        })
    }
}
