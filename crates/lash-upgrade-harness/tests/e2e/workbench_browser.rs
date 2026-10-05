//! S29/L14: the workbench process dies after durable acceptance, and browsers
//! reconnect to the same admitted input, canonical answer and terminal trace.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write as _;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseLease};
use lash_upgrade_harness::e2e::control::ProcessReceipt;
use lash_upgrade_harness::e2e::evidence::Evidence;
use lash_upgrade_harness::e2e::host::{HostAdapter as _, HostCommand};
use lash_upgrade_harness::e2e::host_adapters::process::{HostProcess, ready};
use lash_upgrade_harness::e2e::host_adapters::workbench::WorkbenchHost;
use lash_upgrade_harness::restate_view::RestateView;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;

use super::{artifact, required, turn_journals, write_case_receipt};

pub async fn run() -> Result<()> {
    let directory = std::path::PathBuf::from(required("LASH_E2E_HOST_ARTIFACTS")?);
    std::fs::create_dir_all(&directory)?;
    let gate = required("KILN_GATE_ID")?;
    let port: u16 = required("LASH_E2E_HOST_PORT")?.parse()?;
    let mut lease = CaseLease {
        gate_id: gate.clone(),
        namespace: format!("s29-{gate}"),
        authority: format!("s29-{gate}"),
        directory: directory.clone(),
        postgres_url: None,
        ports: (port..port + 3).collect(),
        deadline: Instant::now() + Duration::from_secs(240),
        processes: Vec::new(),
        cleanup: Vec::new(),
    };
    let python: std::path::PathBuf = required("LASH_E2E_PYTHON")?.into();
    let digest = Command::new("sha256sum").arg(&python).output().await?;
    ensure!(digest.status.success(), "hash S29 interpreter");
    let python = ArtifactIdentity {
        role: "workbench-browser".into(),
        path: python,
        sha256: String::from_utf8(digest.stdout)?
            .split_whitespace()
            .next()
            .context("interpreter digest missing")?
            .into(),
        candidate_sha: required("LASH_E2E_CANDIDATE_SHA")?,
        generation: required("LASH_E2E_HOST_GENERATION")?,
    };
    let script = std::path::PathBuf::from(required("LASH_E2E_REPO")?)
        .join("scripts/e2e-workbench-recovery.py");
    let args = vec![
        script.display().to_string(),
        "--directory".into(),
        directory.display().to_string(),
        "--provider-port".into(),
        (port + 2).to_string(),
    ];
    let mut provider_args = args.clone();
    provider_args.push("--provider".into());
    let mut provider = HostProcess::spawn_with_args(
        &python,
        &mut lease,
        "s29-recorded-provider",
        BTreeMap::new(),
        vec![port + 2],
        &provider_args,
    )
    .await?;
    let mut host = WorkbenchHost::new(
        required("RESTATE_INGRESS_URL")?,
        required("RESTATE_ADMIN_URL")?,
        port,
        port + 1,
    )?
    .configure(BTreeMap::from([
        (
            "AGENT_WORKBENCH_PROVIDER_URL".into(),
            format!("http://127.0.0.1:{}", port + 2),
        ),
        ("OPENROUTER_API_KEY".into(), "s29-local-fixture".into()),
        ("OPENROUTER_MODEL".into(), "openai/gpt-5.4".into()),
        ("OPENROUTER_MODEL_VARIANT".into(), "high".into()),
    ]))?;
    let server_path = required("LASH_RESTATE_SERVER_BIN")?;
    let server = ArtifactIdentity {
        role: "restate-server".into(),
        sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&server_path)?),
        path: server_path.into(),
        candidate_sha: required("LASH_E2E_CANDIDATE_SHA")?,
        generation: required("LASH_E2E_HOST_GENERATION")?,
    };
    let workbench = artifact("WORKBENCH", "workbench")?;
    let view = RestateView::new(&required("RESTATE_ADMIN_URL")?, &lease.namespace)?;
    let mut evidence = Evidence::empty("S29".into());
    evidence.artifacts = vec![workbench.clone(), python.clone(), server];
    let result = async {
        ready(
            &mut provider,
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{}/healthz", port + 2),
            "s29-provider",
            lease.deadline,
        )
        .await?;
        let boot = host.boot(&workbench, &mut lease).await?;
        let mut browser_args = args;
        browser_args.extend(["--base-url".into(), boot.endpoint.clone()]);
        let mut browser = Command::new(&python.path)
            .args(&browser_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(directory.join("s29-browser.stderr"))?)
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;
        let pid = browser.id().context("browser PID missing")?;
        lease.processes.push(ProcessReceipt {
            role: "workbench-browser".into(),
            pid,
            incarnation: 1,
            log: directory.join("s29-browser.log").display().to_string(),
        });
        let mut output = BufReader::new(browser.stdout.take().context("browser stdout")?).lines();
        let mut input = browser.stdin.take().context("browser stdin")?;
        let mut log = File::create(directory.join("s29-browser.log"))?;
        let collected = async {
            while let Some(line) = tokio::time::timeout(
                lease.deadline.saturating_duration_since(Instant::now()),
                output.next_line(),
            )
            .await
            .context("S29 browser watchdog")??
            {
                writeln!(log, "{line}")?;
                if let Some(request) = line.strip_prefix("H6_CONTROL ") {
                    let request: Value = serde_json::from_str(request)?;
                    let response = match host
                        .command(HostCommand::Process {
                            action: request["action"].as_str().context("control action")?.into(),
                            input: request["input"].clone(),
                        })
                        .await
                    {
                        Ok(receipt) => receipt.output,
                        Err(error) => json!({"error":format!("{error:#}")}),
                    };
                    let mut bytes = serde_json::to_vec(&response)?;
                    bytes.push(b'\n');
                    input.write_all(&bytes).await?;
                    input.flush().await?;
                }
            }
            ensure!(browser.wait().await?.success(), "S29 browser oracle failed");
            let score: Value =
                serde_json::from_slice(&std::fs::read(directory.join("scorecard.json"))?)?;
            ensure!(
                score["scenario"] == "S29"
                    && score["selected"] == 1
                    && score["executed"] == 1
                    && score["verdict"] == "PASS",
                "S29 did not execute its complete oracle: {score}"
            );
            anyhow::Ok(())
        }
        .await;
        if browser.try_wait()?.is_none() {
            Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .status()
                .await?;
            browser.wait().await?;
        }
        lease
            .cleanup
            .push(lash_upgrade_harness::e2e::control::CleanupReceipt {
                resource: format!("pid:{pid}"),
                closed: true,
                detail: "browser oracle reaped".into(),
            });
        collected?;
        evidence.journals = turn_journals(&view).await?;
        anyhow::Ok(())
    }
    .await;
    let host_cleanup = host.stop().await;
    let provider_cleanup = provider
        .stop(Instant::now() + Duration::from_secs(10), None)
        .await;
    let mut errors = Vec::new();
    if let Err(error) = &result {
        errors.push(format!("{error:#}"));
    }
    match host.transcript() {
        Ok(observations) => evidence.outputs = observations,
        Err(error) => errors.push(format!("transcript: {error:#}")),
    }
    evidence.cleanup.extend(lease.cleanup.iter().cloned());
    write_case_receipt(
        &directory,
        evidence,
        errors,
        &[("host", &host_cleanup), ("provider", &provider_cleanup)],
    )?;
    result?;
    ensure!(
        host_cleanup?
            .iter()
            .chain(provider_cleanup?.iter())
            .all(|r| r.closed),
        "S29 leaked an owned lifetime"
    );
    Ok(())
}
