//! Optional paid rows retain the existing exact oracles. Collection never
//! turns an absent credential or semantic judge into a passing verdict.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;

use super::process::{HostProcess, ready};
use crate::e2e::case::{ArtifactIdentity, CaseLease};

pub const RLM_ROWS: [&str; 3] = [
    "file-edit-bugfix",
    "missing-helper-file",
    "config-contract-edit",
];
pub const LIVE_ROWS: [&str; 4] = [
    "S35/file-edit-bugfix",
    "S35/missing-helper-file",
    "S35/config-contract-edit",
    "S36/workbench-weather",
];

#[derive(Debug, Serialize, Deserialize)]
pub enum LiveVerdict {
    Passed,
    NotRun { reason: String },
    NeedsJudgement { runbook: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LiveReceipt {
    pub row: String,
    pub selected: usize,
    pub executed: usize,
    pub verdict: LiveVerdict,
    pub evidence: Value,
}

pub fn missing_credentials(directory: &Path) -> Result<Option<Vec<LiveReceipt>>> {
    if std::env::var("OPENROUTER_API_KEY").is_ok_and(|key| !key.trim().is_empty()) {
        return Ok(None);
    }
    let rows: Vec<_> = LIVE_ROWS
        .iter()
        .map(|row| LiveReceipt {
            row: (*row).into(),
            selected: 1,
            executed: 0,
            verdict: LiveVerdict::NotRun {
                reason: "OPENROUTER_API_KEY is absent".into(),
            },
            evidence: Value::Null,
        })
        .collect();
    std::fs::create_dir_all(directory)?;
    std::fs::write(
        directory.join("live-rows.json"),
        serde_json::to_vec_pretty(&rows)?,
    )?;
    Ok(Some(rows))
}

pub struct LiveConfig {
    pub repo: PathBuf,
    pub ingress: String,
    pub admin: String,
    pub port: u16,
    pub model: String,
    pub budget: PathBuf,
    pub output_token_cap: usize,
    pub python: PathBuf,
}

impl LiveConfig {
    /// The case's own copy of the explicit capped account policy. One policy
    /// serves every selected case, so its usage receipt path is relative and
    /// resolves inside the case directory.
    fn environment(&self, lease: &CaseLease) -> Result<(BTreeMap<String, String>, PathBuf)> {
        ensure!(
            std::env::var("OPENROUTER_API_KEY").is_ok_and(|key| !key.trim().is_empty()),
            "paid row requires a credential; use missing_credentials before selecting it"
        );
        let mut budget: Value = serde_json::from_slice(&std::fs::read(&self.budget)?)?;
        ensure!(
            budget["model"] == self.model
                && budget["max_output_tokens"]
                    .as_u64()
                    .is_some_and(|cap| cap >= self.output_token_cap as u64),
            "live row differs from its explicit capped account policy"
        );
        let receipts = Path::new(
            budget["receipts"]
                .as_str()
                .context("live budget has no usage receipt path")?,
        );
        ensure!(
            receipts
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
            "live usage receipt path must be relative to its case"
        );
        let receipts = lease.directory.join(receipts);
        budget["receipts"] = json!(receipts);
        std::fs::create_dir_all(&lease.directory)?;
        let resolved = lease.directory.join("live-budget.json");
        std::fs::write(&resolved, serde_json::to_vec_pretty(&budget)?)?;
        let environment = BTreeMap::from([
            ("RESTATE_INGRESS_URL".into(), self.ingress.clone()),
            ("RESTATE_ADMIN_URL".into(), self.admin.clone()),
            ("RESTATE_AUTHORITY_ID".into(), lease.authority.clone()),
            ("OPENROUTER_MODEL".into(), self.model.clone()),
            (
                "LASH_E2E_LIVE_BUDGET".into(),
                resolved.display().to_string(),
            ),
            (
                "LASH_E2E_OUTPUT_TOKEN_CAP".into(),
                self.output_token_cap.to_string(),
            ),
            ("LASH_E2E_RESTATE_NAMESPACE".into(), lease.namespace.clone()),
        ]);
        Ok((environment, receipts))
    }

    pub async fn rlm_row(
        &self,
        row: &str,
        artifact: &ArtifactIdentity,
        lease: &mut CaseLease,
    ) -> Result<LiveReceipt> {
        ensure!(RLM_ROWS.contains(&row), "unknown RLM workspace row");
        let (environment, usage) = self.environment(lease)?;
        let source = self.repo.join("runbooks/rlm-smoke/cases").join(row);
        let workspace = lease.directory.join("workspace");
        ensure!(!workspace.exists(), "live workspace must start fresh");
        std::fs::create_dir_all(&workspace)?;
        ensure!(
            Command::new("cp")
                .args(["-a"])
                .arg(source.join("workspace/."))
                .arg(&workspace)
                .status()
                .await?
                .success(),
            "copy original RLM workspace"
        );
        let args = vec![
            "--scenario".into(),
            row.into(),
            "--scenario-dir".into(),
            source.display().to_string(),
            "--workspace".into(),
            workspace.display().to_string(),
            "--data-dir".into(),
            lease.directory.join("data").display().to_string(),
            "--artifact-dir".into(),
            lease.directory.join("artifacts").display().to_string(),
            "--session-id".into(),
            lease.namespace.clone(),
            "--port".into(),
            self.port.to_string(),
            "--trace-offset".into(),
            "1000000".into(),
            "--model".into(),
            self.model.clone(),
        ];
        let mut child = HostProcess::spawn_with_args(
            artifact,
            lease,
            "rlm-live",
            environment,
            vec![self.port],
            &args,
        )
        .await?;
        let result = async {
            child.finish(lease.deadline).await?;
            let oracle = Command::new("bash")
                .arg(source.join("check.sh"))
                .arg(&workspace)
                .arg(&source)
                .output()
                .await?;
            std::fs::write(lease.directory.join("oracle.stdout"), &oracle.stdout)?;
            std::fs::write(lease.directory.join("oracle.stderr"), &oracle.stderr)?;
            ensure!(
                oracle.status.success(),
                "original RLM workspace oracle failed"
            );
            let host: Value = serde_json::from_slice(&std::fs::read(
                lease.directory.join("artifacts/host-evidence.json"),
            )?)?;
            ensure!(
                host["turn_succeeded"] == true
                    && host["scenario"] == row
                    && host["code_languages"] == json!(["typescript"]),
                "live host row identity/terminal mismatch"
            );
            let trace = records(&lease.directory.join("artifacts/trace.jsonl"))?;
            no_identical_error_loop(&trace)?;
            let usage: Value = serde_json::from_slice(&std::fs::read(&usage)?)?;
            ensure!(
                usage["calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty()),
                "live row has no actual usage receipts"
            );
            anyhow::Ok(
                json!({"host":host,"usage":usage,"trace":trace,"oracle":"original check.sh"}),
            )
        }
        .await;
        let cleanup = child
            .stop(Instant::now() + Duration::from_secs(20), None)
            .await;
        persist_cleanup(lease, &cleanup)?;
        let evidence = result?;
        cleanup?;
        let receipt = LiveReceipt {
            row: format!("S35/{row}"),
            selected: 1,
            executed: 1,
            verdict: LiveVerdict::Passed,
            evidence,
        };
        write_receipt(lease, &receipt)?;
        Ok(receipt)
    }

    pub async fn weather(
        &self,
        artifact: &ArtifactIdentity,
        lease: &mut CaseLease,
    ) -> Result<LiveReceipt> {
        let (mut environment, usage) = self.environment(lease)?;
        let data = lease.directory.join("data");
        ensure!(!data.exists(), "weather store must start fresh");
        environment.extend(BTreeMap::from([
            (
                "AGENT_WORKBENCH_ADDR".into(),
                format!("127.0.0.1:{}", self.port),
            ),
            (
                "AGENT_WORKBENCH_RESTATE_ADDR".into(),
                format!("127.0.0.1:{}", self.port + 1),
            ),
            (
                "AGENT_WORKBENCH_RESTATE_NAMESPACE".into(),
                lease.namespace.clone(),
            ),
            (
                "AGENT_WORKBENCH_DATA_DIR".into(),
                data.display().to_string(),
            ),
            ("AGENT_WORKBENCH_OPEN".into(), "0".into()),
            (
                "AGENT_WORKBENCH_OUTPUT_TOKEN_CAP".into(),
                self.output_token_cap.to_string(),
            ),
        ]));
        let mut child = HostProcess::spawn(
            artifact,
            lease,
            "weather-workbench",
            environment,
            vec![self.port, self.port + 1],
        )
        .await?;
        let result = async {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .no_proxy()
                .build()?;
            ready(
                &mut child,
                &http,
                &format!("http://127.0.0.1:{}/healthz", self.port),
                "agent-workbench",
                lease.deadline,
            )
            .await?;
            let registration = Command::new(&artifact.path)
                .arg("register-deployment")
                .arg(format!("http://127.0.0.1:{}", self.port + 1))
                .env("RESTATE_INGRESS_URL", &self.ingress)
                .env("RESTATE_ADMIN_URL", &self.admin)
                .env("RESTATE_AUTHORITY_ID", &lease.authority)
                .env("AGENT_WORKBENCH_RESTATE_NAMESPACE", &lease.namespace)
                .output()
                .await?;
            ensure!(
                registration.status.success(),
                "register weather endpoint: {}",
                String::from_utf8_lossy(&registration.stderr)
            );
            let args = vec![
                self.repo
                    .join("scripts/e2e-workbench-weather.py")
                    .display()
                    .to_string(),
                "--base-url".into(),
                format!("http://127.0.0.1:{}", self.port),
                "--directory".into(),
                lease.directory.display().to_string(),
                "--model".into(),
                self.model.clone(),
            ];
            let python_artifact = ArtifactIdentity {
                role: "weather-browser".into(),
                path: self.python.clone(),
                sha256: hash(&self.python).await?,
                candidate_sha: artifact.candidate_sha.clone(),
                generation: artifact.generation.clone(),
            };
            let mut browser = HostProcess::spawn_with_args(
                &python_artifact,
                lease,
                "weather-browser",
                BTreeMap::new(),
                Vec::new(),
                &args,
            )
            .await?;
            let collected = browser.finish(lease.deadline).await;
            let browser_cleanup = browser
                .stop(Instant::now() + Duration::from_secs(10), None)
                .await;
            if let Ok(receipts) = &browser_cleanup {
                lease.cleanup.extend(receipts.clone());
            }
            collected?;
            browser_cleanup?;
            anyhow::Ok(())
        }
        .await;
        let cleanup = child
            .stop(
                Instant::now() + Duration::from_secs(30),
                Some("agent-workbench shutdown complete"),
            )
            .await;
        persist_cleanup(lease, &cleanup)?;
        result?;
        cleanup?;
        let receipt = LiveReceipt {
            row: "S36/workbench-weather".into(),
            selected: 1,
            executed: 1,
            verdict: LiveVerdict::NeedsJudgement {
                runbook: "runbooks/workbench-weather/runbook.md".into(),
            },
            evidence: json!({"directory":lease.directory,"usage":usage,"judge_receipt_required":true}),
        };
        write_receipt(lease, &receipt)?;
        Ok(receipt)
    }
}

async fn hash(path: &Path) -> Result<String> {
    let result = Command::new("sha256sum").arg(path).output().await?;
    ensure!(result.status.success(), "hash browser interpreter");
    Ok(String::from_utf8(result.stdout)?
        .split_whitespace()
        .next()
        .context("digest")?
        .into())
}
fn persist_cleanup(
    lease: &mut CaseLease,
    cleanup: &Result<Vec<crate::e2e::control::CleanupReceipt>>,
) -> Result<()> {
    if let Ok(receipts) = cleanup {
        lease.cleanup.extend(receipts.clone());
    }
    std::fs::write(
        lease.directory.join("cleanup.json"),
        serde_json::to_vec_pretty(&json!({"receipts":lease.cleanup,
        "error":cleanup.as_ref().err().map(ToString::to_string),"processes":lease.processes}))?,
    )?;
    Ok(())
}
fn write_receipt(lease: &CaseLease, receipt: &LiveReceipt) -> Result<()> {
    std::fs::write(
        lease.directory.join("row.json"),
        serde_json::to_vec_pretty(receipt)?,
    )?;
    Ok(())
}
fn records(path: &Path) -> Result<Vec<Value>> {
    std::fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}
fn no_identical_error_loop(trace: &[Value]) -> Result<()> {
    let mut previous = String::new();
    for record in trace
        .iter()
        .filter(|record| record["type"] == "exec_code_completed")
    {
        let raw_error = match &record["error"] {
            Value::Null => String::new(),
            Value::String(error) => error.clone(),
            error => serde_json::to_string(error)?,
        };
        let error = raw_error.split_whitespace().collect::<Vec<_>>().join(" ");
        ensure!(
            error.is_empty() || error != previous,
            "repeated identical execution error: {error}"
        );
        previous = error;
    }
    Ok(())
}
