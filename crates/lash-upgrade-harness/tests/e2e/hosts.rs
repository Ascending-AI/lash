//! H6: R7/L03/L08/L21 through a separate external consumer process and
//! its real HTTP contract, with the controller owning its lifetime.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseLease};
use lash_upgrade_harness::e2e::host::{HostAdapter as _, HostCommand};
use lash_upgrade_harness::e2e::host_adapters::consumer::ConsumerHost;
use lash_upgrade_harness::e2e::host_adapters::slack::SlackHost;
use serde_json::json;

fn required(name: &str) -> Result<String> {
    std::env::var(name)
        .with_context(|| format!("{name} is required; missing setup is not a passing scenario"))
}

// Each paid workspace/session is independently selectable and counted.
macro_rules! paid_row {
    ($name:ident, $row:literal) => {
        #[test]
        #[ignore = "optional capped live provider row; controller preflight records absent credentials as NotRun"]
        fn $name() -> Result<()> {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all().thread_stack_size(8 * 1024 * 1024).build()?
                .block_on(live_row($row))
        }
    };
}
paid_row!(s35_file_edit_bugfix, "S35/file-edit-bugfix");
paid_row!(s35_missing_helper_file, "S35/missing-helper-file");
paid_row!(s35_config_contract_edit, "S35/config-contract-edit");
paid_row!(s36_slack_nonce, "S36/slack-nonce");
paid_row!(s36_workbench_weather, "S36/workbench-weather");

async fn live_row(row: &str) -> Result<()> {
    use lash_upgrade_harness::e2e::host_adapters::live::{LiveConfig, missing_credentials};
    let root = PathBuf::from(required("LASH_E2E_HOST_ARTIFACTS")?);
    ensure!(
        missing_credentials(&root)?.is_none(),
        "live row is NotRun; absent credentials cannot pass"
    );
    let gate = required("KILN_GATE_ID")?;
    let port: u16 = required("LASH_E2E_HOST_PORT")?.parse()?;
    std::fs::create_dir_all(&root)?;
    let mut lease = CaseLease {
        gate_id: gate.clone(),
        namespace: format!("h6-{gate}-{}", row.replace('/', "-")),
        authority: format!("h6-{gate}"),
        directory: root,
        postgres_url: None,
        ports: (port..port + 7).collect(),
        deadline: Instant::now() + Duration::from_secs(600),
        processes: Vec::new(),
        cleanup: Vec::new(),
    };
    let config = LiveConfig {
        repo: required("LASH_E2E_REPO")?.into(),
        ingress: required("RESTATE_INGRESS_URL")?,
        admin: required("RESTATE_ADMIN_URL")?,
        port,
        model: required("OPENROUTER_MODEL")?,
        budget: required("LASH_E2E_LIVE_BUDGET")?.into(),
        output_token_cap: required("LASH_E2E_OUTPUT_TOKEN_CAP")?.parse()?,
        python: required("LASH_E2E_PYTHON")?.into(),
    };
    let receipt = match row {
        "S36/slack-nonce" => {
            config
                .slack_nonce(
                    &artifact("SLACK_PLATFORM", "slack-platform")?,
                    &artifact("SLACK_LIVE", "slack-live")?,
                    required("LASH_LIVE_E2E_MAX_SPEND_USD")?.parse()?,
                    &mut lease,
                )
                .await?
        }
        "S36/workbench-weather" => {
            config
                .weather(&artifact("WORKBENCH", "workbench")?, &mut lease)
                .await?
        }
        _ => {
            config
                .rlm_row(
                    row.strip_prefix("S35/").context("unknown paid row")?,
                    &artifact("RLM_HOST", "rlm-host")?,
                    &mut lease,
                )
                .await?
        }
    };
    ensure!(
        receipt.row == row && receipt.selected == 1 && receipt.executed == 1,
        "live collection did not execute its selected row"
    );
    ensure!(
        lease.cleanup.iter().all(|receipt| receipt.closed),
        "live host lifetime remains open"
    );
    Ok(())
}

fn artifact(prefix: &str, role: &str) -> Result<ArtifactIdentity> {
    Ok(ArtifactIdentity {
        role: role.into(),
        path: required(&format!("LASH_E2E_{prefix}_BIN"))?.into(),
        sha256: required(&format!("LASH_E2E_{prefix}_SHA256"))?,
        candidate_sha: required("LASH_E2E_CANDIDATE_SHA")?,
        generation: required("LASH_E2E_HOST_GENERATION")?,
    })
}

#[test]
#[ignore = "prebuilt Slack hosts, Playwright and private real Restate supplied by the E2E controller"]
fn s28_slack_mcp_peer_restart() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(slack("S28"))
}

#[test]
#[ignore = "prebuilt Slack hosts, Playwright and private real Restate supplied by the E2E controller"]
fn s29_slack_bot_kill_after_acceptance() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(slack("S29"))
}

async fn slack(scenario: &str) -> Result<()> {
    let gate = required("KILN_GATE_ID")?;
    let root = PathBuf::from(required("LASH_E2E_HOST_ARTIFACTS")?);
    let port: u16 = required("LASH_E2E_HOST_PORT")?.parse()?;
    let mut lease = CaseLease {
        gate_id: gate.clone(),
        namespace: format!("h6-{gate}"),
        authority: format!("h6-{gate}"),
        directory: root.clone(),
        postgres_url: None,
        ports: (port..port + 4).collect(),
        deadline: Instant::now() + Duration::from_secs(360),
        processes: Vec::new(),
        cleanup: Vec::new(),
    };
    let mut host = SlackHost::new(
        artifact("SLACK_PLATFORM", "slack-platform")?,
        artifact("SLACK_MCP", "slack-http-mcp")?,
        artifact("SLACK_STDIO_MCP", "slack-stdio-mcp")?,
        required("RESTATE_INGRESS_URL")?,
        required("RESTATE_ADMIN_URL")?,
        port,
        required("LASH_E2E_REPO")?.into(),
        required("LASH_E2E_PYTHON")?.into(),
    )?;
    let result = async {
        let ready = host
            .boot(&artifact("SLACK_BOT", "slack-bot")?, &mut lease)
            .await?;
        let score = host.oracle(scenario).await?;
        std::fs::write(
            root.join("host-evidence.json"),
            serde_json::to_vec_pretty(&json!({
                "scenario":scenario,"selected":1,"executed":1,"ready":ready,
                "scorecard":score,"transcript":host.transcript()?
            }))?,
        )?;
        anyhow::Ok(())
    }
    .await;
    let cleanup = host.stop().await;
    result?;
    ensure!(
        cleanup?.iter().all(|receipt| receipt.closed),
        "Slack lifetime was not closed"
    );
    Ok(())
}

#[test]
#[ignore = "prebuilt external consumer and private real Restate supplied by the E2E controller"]
fn s30_external_consumer_accept_follow_cancel() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(s30())
}

async fn s30() -> Result<()> {
    let gate = required("KILN_GATE_ID")?;
    let root = PathBuf::from(required("LASH_E2E_HOST_ARTIFACTS")?);
    let artifact = ArtifactIdentity {
        role: "external-consumer".into(),
        path: required("LASH_E2E_CONSUMER_BIN")?.into(),
        sha256: required("LASH_E2E_CONSUMER_SHA256")?,
        candidate_sha: required("LASH_E2E_CANDIDATE_SHA")?,
        generation: required("LASH_E2E_CONSUMER_GENERATION")?,
    };
    let port: u16 = required("LASH_E2E_HOST_PORT")?.parse()?;
    let mut lease = CaseLease {
        gate_id: gate.clone(),
        namespace: format!("h6-{gate}"),
        authority: format!("h6-{gate}"),
        directory: root.clone(),
        postgres_url: None,
        ports: vec![port, port + 1],
        deadline: Instant::now() + Duration::from_secs(90),
        processes: Vec::new(),
        cleanup: Vec::new(),
    };
    let mut host = ConsumerHost::new(
        required("RESTATE_INGRESS_URL")?,
        required("RESTATE_ADMIN_URL")?,
        port,
        port + 1,
    )?;
    let scenario = async {
        let ready = host.boot(&artifact, &mut lease).await?;
        ensure!(ready.protocol == 7, "the real deployment did not negotiate V7");
        // submit returns durable admission, not a terminal. The host discards
        // the SendHandle before the tool is released, and a fresh HTTP
        // follower reattaches using that original input identity.
        let accepted = host.command(HostCommand::Submit { session: "consumer-follow".into(),
            idempotency_key: "consumer-follow-run".into(), input: json!("hold:follow") }).await?;
        let entered = host.body_entered("hold:follow").await?;
        ensure!(entered["call_id"].as_str().is_some_and(|id| !id.is_empty()), "body receipt has no original tool identity");
        let run = host.binding(&accepted.work.ingress).await?;
        ensure!(!run.is_empty(), "durable acceptance never reached a Run");
        host.release("hold:follow").await?;
        let first = host.command(HostCommand::Attach { run: accepted.work.ingress.clone() }).await?;
        let second = host.command(HostCommand::Attach { run: accepted.work.ingress.clone() }).await?;
        let decode = |output: &serde_json::Value| -> Result<lash::remote::turn_result::RemoteSendOutcome> {
            let outcome = serde_json::from_value::<lash::remote::turn_result::RemoteSendOutcome>(output.clone())?;
            outcome.validate()?;
            Ok(outcome)
        };
        let first = decode(&first.output)?;
        let second = decode(&second.output)?;
        ensure!(first.status() == lash::remote::turn_result::RemoteTurnStatus::Answered, "reattached consumer did not answer: {first:?}");
        ensure!(first.report().map(|report| report.assistant_output.safe_text.as_str()) == Some("echo:hold:follow"), "reattached reply differs from tool input");
        ensure!(first.report().map(|report| &report.turn_id) == second.report().map(|report| &report.turn_id), "reattachment acquired a second Run");
        ensure!(first.report().map(|report| &report.assistant_output) == second.report().map(|report| &report.assistant_output), "durable result changed between followers");
        let late_cancel = host.command(HostCommand::Cancel { run: accepted.work.ingress.clone() }).await?;
        ensure!(late_cancel.output["receipt"].as_str().is_some_and(|receipt| receipt.contains("AlreadySettled")), "cancel after terminal changed the decision");

        let cancelled = host.command(HostCommand::Submit { session: "consumer-cancel".into(),
            idempotency_key: "consumer-cancel-run".into(), input: json!("hold:cancel") }).await?;
        host.body_entered("hold:cancel").await?;
        host.command(HostCommand::Cancel { run: cancelled.work.ingress.clone() }).await?;
        // The body cooperatively wakes on cancel; its late answer cannot
        // change the recorded cancellation decision or create another owner.
        let terminal = host.command(HostCommand::Attach { run: cancelled.work.ingress.clone() }).await?;
        let terminal = decode(&terminal.output)?;
        ensure!(terminal.status() == lash::remote::turn_result::RemoteTurnStatus::Cancelled, "cancel-before-terminal failed: {terminal:?}");

        let operation = host.command(HostCommand::Operation { session: "consumer-follow".into(),
            input: json!({"id":"consumer-task", "text":"hold:task"}) }).await?;
        host.body_entered("hold:task").await?;
        host.release("hold:task").await?;
        let operation = host.command(HostCommand::Attach { run: operation.work.run }).await?;
        ensure!(operation.output["output"] == "hold:task", "explicit operation completion lost its result");
        let bodies = host.body_receipts().await?;
        let bodies = bodies.as_array().context("body receipts must be an array")?;
        ensure!(bodies.len() == 3, "expected one body per accepted turn/task, got {}", bodies.len());
        ensure!(bodies.iter().filter(|body| body["key"] == "hold:follow").count() == 1, "follower restarted the tool body");
        std::fs::write(root.join("s30-evidence.json"), serde_json::to_vec_pretty(&json!({
            "scenario":"S30", "rules":["R7","L03","L08","L21"], "selected":1,"executed":1,
            "ready":ready,"run":run,"outcome":first,"cancelled":terminal,"bodies":bodies,"transcript":host.transcript()?
        }))?)?;
        anyhow::Ok(())
    }.await;
    let cleanup = host.stop().await;
    // Cleanup runs before propagating any oracle error, preserving the
    // first failure alongside cleanup evidence.
    std::fs::create_dir_all(&root)?;
    std::fs::write(
        root.join("s30-cleanup.json"),
        serde_json::to_vec_pretty(&json!({
            "processes":lease.processes,"receipts":cleanup.as_ref().ok(),"error":cleanup.as_ref().err().map(ToString::to_string)
        }))?,
    )?;
    scenario?;
    let cleanup = cleanup?;
    ensure!(
        !cleanup.is_empty() && cleanup.iter().all(|receipt| receipt.closed),
        "consumer lifetime was not closed"
    );
    Ok(())
}
