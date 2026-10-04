//! S28 owns a real, independently restartable workbench MCP peer.
use super::*;
use std::{fs::File, io::Write as _, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    process::Command,
};
impl WorkbenchHost {
    pub async fn boot_mcp(
        &mut self,
        artifact: &ArtifactIdentity,
        lease: &mut CaseLease,
    ) -> Result<()> {
        self.mcp_artifact = Some(artifact.clone());
        self.mcp = Some(
            HostProcess::spawn_with_args(
                artifact,
                lease,
                "workbench-http-mcp",
                BTreeMap::from([
                    (
                        "AGENT_WORKBENCH_MCP_ADDR".into(),
                        format!("127.0.0.1:{}", self.http_port + 2),
                    ),
                    (
                        "AGENT_WORKBENCH_MCP_GATE_DIR".into(),
                        lease.directory.join("mcp-gates").display().to_string(),
                    ),
                ]),
                vec![self.http_port + 2],
                &["mcp-fixture".into(), "http".into()],
            )
            .await?,
        );
        loop {
            self.mcp
                .as_mut()
                .context("missing MCP peer")?
                .check_alive()?;
            if tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, self.http_port + 2))
                .await
                .is_ok()
            {
                return Ok(());
            }
            ensure!(
                Instant::now() < lease.deadline,
                "workbench MCP peer did not bind"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    async fn mcp_action(&mut self, request: Value, lease: &mut CaseLease) -> Result<Value> {
        match request["action"]
            .as_str()
            .context("missing controller action")?
        {
            "mcp-restart" => {
                ensure!(
                    !request["event"].as_str().unwrap_or_default().is_empty(),
                    "peer fault lacks admitted input identity"
                );
                let entered: u32 =
                    std::fs::read_to_string(lease.directory.join("mcp-gates/badge-entered"))?
                        .trim()
                        .parse()?;
                let process = self.mcp.as_mut().context("no MCP peer")?;
                ensure!(
                    entered == process.receipt.pid,
                    "MCP barrier belongs to another incarnation"
                );
                let prior = process.receipt.clone();
                process.kill().await?;
                self.mcp_cleanup.extend(
                    process
                        .stop(Instant::now() + Duration::from_secs(10), None)
                        .await?,
                );
                self.mcp = None;
                let artifact = self.mcp_artifact.clone().context("missing MCP artifact")?;
                self.boot_mcp(&artifact, lease).await?;
                Ok(
                    json!({"reaped":true,"ready":true,"process":prior,"successor":self.mcp.as_ref().map(|p| &p.receipt),"event":request["event"],"barrier":"badge-body-entered"}),
                )
            }
            "tool-journal" => {
                // The oracle read this executor from the host's business
                // store. Use the production key function, then read the
                // actual V7 invocation in this case's namespace.
                let session = request["session"]
                    .as_str()
                    .context("journal session absent")?;
                let run = request["run"].as_str().context("journal Run absent")?;
                let executor: lash_core::store::RunExecutor =
                    serde_json::from_value(request["executor"].clone())?;
                let lash_core::store::RunExecutor::Run { admission } = executor else {
                    bail!("tool Run has no Restate executor admission");
                };
                let (shift, ordinal) = admission
                    .as_str()
                    .rsplit_once('#')
                    .context("executor admission lacks its ordinal")?;
                let key = lash_restate::turn_invocation_key(
                    &lash_core::engine::ShiftRequest {
                        session: lash_core::SessionId::parse(session)?,
                        request: lash_core::engine::ShiftRequestId::new(shift),
                        intended_lane: None,
                    },
                    ordinal.parse()?,
                );
                let view = crate::restate_view::RestateView::new(&self.admin, &lease.namespace)?;
                let rows: Vec<Value> = view.query(&format!(
                    "SELECT id,target_service_name,pinned_service_protocol_version FROM sys_invocation WHERE target_service_key='{}' AND target_handler_name='run'",
                    key.replace('\'', "''"))).await?;
                let prefix = view.service_name("LashTurn");
                let rows: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        row["target_service_name"].as_str().is_some_and(|name| {
                            name == prefix || name.starts_with(&format!("{prefix}_g"))
                        })
                    })
                    .collect();
                ensure!(
                    rows.len() == 1,
                    "tool Run has no unique actual invocation: {rows:?}"
                );
                ensure!(
                    rows[0]["pinned_service_protocol_version"] == 7,
                    "tool Run did not negotiate V7"
                );
                let invocation = rows[0]["id"].as_str().context("invocation has no id")?;
                let work = WorkIdentity {
                    ingress: request["source"].as_str().context("source absent")?.into(),
                    run: run.into(),
                    segment: invocation.into(),
                    call: None,
                    ordinal: None,
                };
                let journals = view.journal(&work, invocation, 7).await?;
                let mut materials = BTreeMap::new();
                let mut outcomes = Vec::new();
                for fact in &journals {
                    use crate::e2e::evidence::DecodedRecord;
                    use lash_core_store::tool_run::{MaterialEntry, MaterialRole};
                    let entries = match &fact.decoded {
                        Some(DecodedRecord::Run(entry)) => &entry.materials,
                        Some(DecodedRecord::Attempt(entry)) => &entry.materials,
                        _ => continue,
                    };
                    for entry in entries {
                        if let MaterialEntry::Available { reference, payload } = entry {
                            ensure!(
                                payload.format == 1
                                    && payload.reference(reference.location.clone())? == *reference,
                                "native material does not match its canonical reference"
                            );
                            materials.insert(
                                reference.digest.to_string(),
                                json!({"reference":reference,"payload":payload}),
                            );
                        }
                    }
                    if let Some(DecodedRecord::Attempt(entry)) = &fact.decoded {
                        let output = match &entry.result {
                            lash_core_store::tool_run::AttemptResult::Done { output }
                            | lash_core_store::tool_run::AttemptResult::Failed { output, .. } => {
                                output
                            }
                            _ => continue,
                        };
                        let material = entries
                            .iter()
                            .find_map(|entry| match entry {
                                MaterialEntry::Available { reference, payload }
                                    if reference == output =>
                                {
                                    Some(payload)
                                }
                                _ => None,
                            })
                            .context("attempt output has no retained material")?;
                        ensure!(
                            material.role == MaterialRole::AttemptOutput,
                            "attempt output has another material role"
                        );
                        let capture: lash_core::tool_dispatch::SingletonCapture =
                            serde_json::from_str(&material.text)?;
                        let captured: Value = serde_json::from_str(
                            capture
                                .output()
                                .context("fixture attempt has no native output")?,
                        )?;
                        let output: lash_core::ToolCallOutput =
                            serde_json::from_value(captured["output"].clone())?;
                        outcomes.push(json!({"call_id":entry.call_id,"attempt":entry.attempt,"output":output,"capture":capture}));
                    }
                }
                Ok(
                    json!({"work":work,"executor":request["executor"],"invocation":rows[0],
                    "journals":journals,"materials":materials,"outcomes":outcomes}),
                )
            }
            action => bail!("unknown MCP action {action}"),
        }
    }
    pub async fn mcp_oracle(
        &mut self,
        repo: &std::path::Path,
        python: &std::path::Path,
        lease: &mut CaseLease,
    ) -> Result<Value> {
        let mut command = Command::new(python);
        command
            .arg(repo.join("examples/agent-workbench/tests/mcp_peer_restart.py"))
            .args([
                "--base-url",
                &self.base(),
                "--directory",
                &lease.directory.display().to_string(),
                "--mcp-url",
                &format!("http://127.0.0.1:{}/mcp", self.http_port + 2),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(lease.directory.join("mcp-oracle-stderr.log"))?)
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let pid = child.id().context("MCP oracle PID missing")?;
        lease.processes.push(crate::e2e::control::ProcessReceipt {
            role: "workbench-browser-oracle".into(),
            pid,
            incarnation: 1,
            log: lease.directory.join("mcp-oracle.log").display().to_string(),
        });
        let mut lines = BufReader::new(child.stdout.take().context("MCP oracle output")?).lines();
        let mut input = child.stdin.take().context("MCP oracle input")?;
        let mut log = File::create(lease.directory.join("mcp-oracle.log"))?;
        let result = async {
            while let Some(line) = tokio::time::timeout(
                lease.deadline.saturating_duration_since(Instant::now()),
                lines.next_line(),
            )
            .await
            .context("MCP oracle watchdog")??
            {
                writeln!(log, "{line}")?;
                if let Some(request) = line.strip_prefix("H6_CONTROL ") {
                    let request: Value = serde_json::from_str(request)?;
                    let receipt = match self.mcp_action(request, lease).await {
                        Ok(receipt) => receipt,
                        Err(error) => json!({"error":format!("{error:#}")}),
                    };
                    let mut bytes = serde_json::to_vec(&receipt)?;
                    bytes.push(b'\n');
                    input.write_all(&bytes).await?;
                    input.flush().await?;
                }
            }
            ensure!(
                child.wait().await?.success(),
                "workbench MCP oracle failed; artifacts {}",
                lease.directory.display()
            );
            let score: Value =
                serde_json::from_slice(&std::fs::read(lease.directory.join("scorecard.json"))?)?;
            ensure!(
                score["selected"] == 1 && score["executed"] == 1 && score["verdict"] == "PASS",
                "S28 did not execute: {score}"
            );
            Ok(score)
        }
        .await;
        if result.is_err() {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .status()
                .await;
        }
        let reaped = child.wait().await;
        self.mcp_cleanup.push(CleanupReceipt {
            resource: format!("workbench-browser-oracle:{pid}"),
            closed: reaped.is_ok(),
            detail: "browser oracle reaped after closing its contexts".into(),
        });
        reaped.context("reap MCP oracle")?;
        result
    }
}
