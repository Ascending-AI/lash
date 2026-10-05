//! H6: R7/L03/L08/L21 through a separate external consumer process and
//! its real HTTP contract, with the controller owning its lifetime.
mod workbench_browser;
mod workbench_provider;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseLease};
use lash_upgrade_harness::e2e::control::{CleanupReceipt, WorkIdentity};
use lash_upgrade_harness::e2e::evidence::{CaseReceipt, Evidence, JournalFact, Verdict};
use lash_upgrade_harness::e2e::host::{HostAdapter as _, HostCommand};
use lash_upgrade_harness::e2e::host_adapters::consumer::ConsumerHost;
use lash_upgrade_harness::restate_view::RestateView;
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

/// Fold teardown results into the evidence and write the CaseReceipt. Returns
/// the final error list so callers keep their own propagation order.
pub(crate) fn write_case_receipt(
    directory: &Path,
    mut evidence: Evidence,
    mut errors: Vec<String>,
    cleanups: &[(&str, &Result<Vec<CleanupReceipt>>)],
) -> Result<Vec<String>> {
    for (resource, cleanup) in cleanups {
        match cleanup {
            Ok(receipts) => evidence.cleanup.extend(receipts.iter().cloned()),
            Err(error) => {
                errors.push(format!("{resource} cleanup: {error:#}"));
                evidence.cleanup.push(CleanupReceipt {
                    resource: (*resource).into(),
                    closed: false,
                    detail: format!("{error:#}"),
                });
            }
        }
    }
    if evidence.cleanup.is_empty() || evidence.cleanup.iter().any(|receipt| !receipt.closed) {
        errors.push("case leaked owned resources".into());
    }
    let verdict = if errors.is_empty() {
        Verdict::Passed
    } else {
        Verdict::Failed {
            reason: errors.join("; "),
        }
    };
    CaseReceipt { evidence, verdict }.write(directory)?;
    Ok(errors)
}

/// ingress and run carry the LashTurn workflow (admission) key.
pub(crate) async fn turn_journals(view: &RestateView) -> Result<Vec<JournalFact>> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: String,
        target_service_key: String,
        target_service_name: String,
        pinned_service_protocol_version: Option<u32>,
    }
    let prefix = view.service_name("LashTurn");
    let rows: Vec<Row> = view
        .query(
            "SELECT id, target_service_key, target_service_name, \
             pinned_service_protocol_version FROM sys_invocation \
             WHERE target_handler_name='run'",
        )
        .await?;
    let rows: Vec<Row> = rows
        .into_iter()
        .filter(|row| {
            row.target_service_name == prefix
                || row.target_service_name.starts_with(&format!("{prefix}_g"))
        })
        .collect();
    ensure!(
        !rows.is_empty(),
        "no V7 LashTurn journal in the case namespace"
    );
    let mut journals = Vec::new();
    for row in rows {
        let key = row.target_service_key;
        let work = WorkIdentity {
            ingress: key.clone(),
            run: key,
            segment: row.id.clone(),
            call: None,
            ordinal: None,
        };
        journals.extend(
            view.journal(
                &work,
                &row.id,
                row.pinned_service_protocol_version.unwrap_or(0),
            )
            .await?,
        );
    }
    Ok(journals)
}

#[test]
#[ignore = "prebuilt external consumer and private real Restate supplied by the E2E controller"]
fn s30_external_consumer_accept_follow_cancel() -> Result<()> {
    use lash_upgrade_harness::e2e::case::{Leg, Permutation, StoreKind};
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(s30(Permutation::provisioned(
            StoreKind::SqliteMemory,
            Leg::Live,
        )?
        .leg))
}

#[test]
#[ignore = "prebuilt external consumer and private real Restate supplied by the E2E controller"]
fn s30_external_consumer_accept_follow_cancel_replay() -> Result<()> {
    use lash_upgrade_harness::e2e::case::{Leg, Permutation, StoreKind};
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(s30(Permutation::provisioned(
            StoreKind::SqliteMemory,
            Leg::Replay,
        )?
        .leg))
}

async fn s30(leg: lash_upgrade_harness::e2e::case::Leg) -> Result<()> {
    let gate = required("KILN_GATE_ID")?;
    let root = PathBuf::from(required("LASH_E2E_HOST_ARTIFACTS")?);
    let artifact = ArtifactIdentity {
        role: "external-consumer".into(),
        path: required("LASH_E2E_CONSUMER_BIN")?.into(),
        sha256: required("LASH_E2E_CONSUMER_SHA256")?,
        candidate_sha: required("LASH_E2E_CANDIDATE_SHA")?,
        generation: required("LASH_E2E_CONSUMER_GENERATION")?,
    };
    let server_path = required("LASH_RESTATE_SERVER_BIN")?;
    let server = ArtifactIdentity {
        role: "restate-server".into(),
        sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&server_path)?),
        path: server_path.into(),
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
    let mut evidence = Evidence::empty("S30".into());
    evidence.artifacts = vec![artifact.clone(), server];
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
        let binding = host.binding_receipt(&accepted.work.ingress).await?;
        let key = binding["invocation_key"].as_str().context("consumer has no retained executor admission")?;
        let view = lash_upgrade_harness::restate_view::RestateView::new(&required("RESTATE_ADMIN_URL")?, &lease.namespace)?;
        let prefix = view.service_name("LashTurn");
        let invocations: Vec<serde_json::Value> = view.query(&format!("SELECT id,target_service_name,pinned_service_protocol_version FROM sys_invocation WHERE target_service_key='{}' AND target_handler_name='run'", key.replace('\'', "''"))).await?;
        let invocations: Vec<_> = invocations.iter().filter(|row| row["target_service_name"].as_str().is_some_and(|name| name == prefix || name.starts_with(&format!("{prefix}_g")))).collect();
        ensure!(invocations.len() == 1, "consumer public Run has no unique actual invocation: {invocations:?}");
        let invocation = invocations[0]["id"].as_str().context("invocation has no id")?;
        ensure!(invocations[0]["pinned_service_protocol_version"] == 7,"consumer invocation did not negotiate V7");
        let work = lash_upgrade_harness::e2e::control::WorkIdentity {
            ingress:accepted.work.ingress.clone(),run:run.clone(),segment:invocation.into(),
            call:Some(entered["call_id"].as_str().context("body call absent")?.into()),
            ordinal:Some(entered["attempt"].as_u64().context("body ordinal absent")?.try_into()?),
        };
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
        let cancellation = host.command(HostCommand::Cancel { run: cancelled.work.ingress.clone() }).await?;
        ensure!(cancellation.output["receipt"].as_str().is_some_and(|receipt| receipt.starts_with("Requested")),"cancel-before-terminal did not record its chosen decision: {cancellation:?}");
        // Deliver the held body after the cancellation is recorded. Its late
        // answer cannot change that decision or create another owner.
        host.release("hold:cancel").await?;
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
        let journals = loop {
            let journals = view.journal(&work, invocation, ready.protocol).await?;
            if journals.iter().any(|fact| matches!(&fact.decoded,
                Some(lash_upgrade_harness::e2e::evidence::DecodedRecord::Attempt(entry))
                if Some(entry.call_id.to_string()) == work.call)) { break journals; }
            ensure!(Instant::now() < lease.deadline,"original consumer Attempt receipt was not independently journaled");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        evidence.stores = vec![binding, invocations[0].clone()];
        evidence.effects = bodies.clone();
        evidence.journals = journals;
        anyhow::Ok(())
    }.await;
    // Prove the leg on the served Restate's own metrics before teardown.
    let base: u16 = required("LASH_E2E_PORT_BASE")?.parse()?;
    let leg_observation = lash_upgrade_harness::e2e::cluster::observe_leg(
        leg,
        &[format!("http://127.0.0.1:{}/metrics", base + 47)],
        &root,
    )
    .await;
    if let Ok(receipt) = &leg_observation {
        std::fs::write(
            root.join("s30-leg-observation.json"),
            serde_json::to_vec_pretty(receipt)?,
        )?;
    }
    // Cleanup runs before propagating any oracle error, preserving the
    // first failure alongside cleanup evidence.
    let cleanup = host.stop().await;
    let mut errors = Vec::new();
    if let Err(error) = &scenario {
        errors.push(format!("{error:#}"));
    }
    match host.transcript() {
        Ok(observations) => evidence.outputs = observations,
        Err(error) => errors.push(format!("transcript: {error:#}")),
    }
    let errors = write_case_receipt(
        &lease.directory,
        evidence,
        errors,
        &[("consumer", &cleanup)],
    )?;
    scenario?;
    leg_observation?;
    cleanup?;
    ensure!(errors.is_empty(), "S30 case receipt recorded a failure");
    Ok(())
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by the E2E controller"]
fn s29_workbench_kill_after_acceptance() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(workbench_browser::run())
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by a Kiln gate"]
fn s26_workbench_rate_limit_and_observer_reconnect_commit_one_answer() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(workbench_provider::run(
            &workbench_provider::S26_RATE_LIMIT,
            lash_upgrade_harness::e2e::case::Leg::Live,
        ))
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by a Kiln gate"]
fn s26_workbench_rate_limit_and_observer_reconnect_commit_one_answer_replay() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(workbench_provider::run(
            &workbench_provider::S26_RATE_LIMIT,
            lash_upgrade_harness::e2e::case::Leg::Replay,
        ))
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by a Kiln gate"]
fn s26_workbench_partial_stream_disconnect_refuses_unsafe_regeneration() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(workbench_provider::run(
            &workbench_provider::S26_PARTIAL_DISCONNECT,
            lash_upgrade_harness::e2e::case::Leg::Live,
        ))
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by a Kiln gate"]
fn s26_workbench_partial_stream_disconnect_refuses_unsafe_regeneration_replay() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(workbench_provider::run(
            &workbench_provider::S26_PARTIAL_DISCONNECT,
            lash_upgrade_harness::e2e::case::Leg::Replay,
        ))
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by a Kiln gate"]
fn s27_workbench_authentication_failure_permits_the_next_run() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(workbench_provider::run(
            &workbench_provider::S27_AUTH_NEXT_RUN,
            lash_upgrade_harness::e2e::case::Leg::Live,
        ))
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by a Kiln gate"]
fn s27_workbench_authentication_failure_permits_the_next_run_replay() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(workbench_provider::run(
            &workbench_provider::S27_AUTH_NEXT_RUN,
            lash_upgrade_harness::e2e::case::Leg::Replay,
        ))
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by a Kiln gate"]
fn s18_workbench_cancel_suspended_application_timer() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(workbench_provider::run(
            &workbench_provider::S18_APPLICATION_TIMER,
            lash_upgrade_harness::e2e::case::Leg::Live,
        ))
}

#[test]
#[ignore = "prebuilt workbench, Playwright and private Restate supplied by a Kiln gate"]
fn s28_workbench_mcp_peer_restart() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(s28_workbench())
}
async fn s28_workbench() -> Result<()> {
    use lash_upgrade_harness::e2e::{
        cluster::{ClusterControl as _, LocalCluster},
        host_adapters::workbench::WorkbenchHost,
    };
    use std::collections::BTreeMap;
    let root = PathBuf::from(required("LASH_E2E_HOST_ARTIFACTS")?);
    let deadline = Instant::now() + Duration::from_secs(360);
    let mut lease = CaseLease::new("s28", root.join("s28"), deadline)?;
    let port: u16 = required("LASH_E2E_PORT_BASE")?.parse()?;
    lease.ports.extend([port + 20, port + 21, port + 22]);
    let candidate = required("LASH_E2E_CANDIDATE_SHA")?;
    let identity = |role: &str, path: PathBuf| -> Result<ArtifactIdentity> {
        Ok(ArtifactIdentity {
            role: role.into(),
            sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?),
            path,
            candidate_sha: candidate.clone(),
            generation: lash_restate::JOURNAL_LOGIC_EPOCH.to_string(),
        })
    };
    let workbench = identity("workbench", required("LASH_E2E_WORKBENCH_BIN")?.into())?;
    let server = identity(
        "restate-server",
        required("LASH_RESTATE_SERVER_BIN")?.into(),
    )?;
    let mut cluster = LocalCluster::new(port, deadline);
    let environment = BTreeMap::from([
        (
            "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO".into(),
            "mcp-fixture".into(),
        ),
        (
            "AGENT_WORKBENCH_SEARCH_MCP_URL".into(),
            "http://127.0.0.1:1/mcp".into(),
        ),
        (
            "AGENT_WORKBENCH_MCP_FIXTURE_BIN".into(),
            workbench.path.display().to_string(),
        ),
        (
            "AGENT_WORKBENCH_MCP_PROVIDER_LOG".into(),
            lease
                .directory
                .join("provider-requests.jsonl")
                .display()
                .to_string(),
        ),
        (
            "AGENT_WORKBENCH_MCP_STDIO_PID".into(),
            lease.directory.join("stdio-peer.pid").display().to_string(),
        ),
    ]);
    let mut host = WorkbenchHost::new(
        format!("http://127.0.0.1:{}", port),
        format!("http://127.0.0.1:{}", port + 1),
        port + 20,
        port + 21,
    )?
    .configure(environment)?;
    let view = RestateView::new(&format!("http://127.0.0.1:{}", port + 1), &lease.namespace)?;
    let mut evidence = Evidence::empty("S28".into());
    evidence.artifacts = vec![workbench.clone(), server.clone()];
    let result = async {
        let boot = cluster.boot(&server, 1, &mut lease).await?;
        std::fs::write(
            lease.directory.join("cluster-boot.json"),
            serde_json::to_vec_pretty(&boot)?,
        )?;
        host.boot_mcp(&workbench, &mut lease).await?;
        host.boot(&workbench, &mut lease).await?;
        host.mcp_oracle(
            &PathBuf::from(required("LASH_E2E_REPO")?),
            &PathBuf::from(required("LASH_E2E_PYTHON")?),
            &mut lease,
        )
        .await?;
        evidence.journals = turn_journals(&view).await?;
        anyhow::Ok(())
    }
    .await;
    let host_cleanup = host.stop().await;
    let cluster_cleanup = cluster.finish().await;
    let mut errors = Vec::new();
    if let Err(error) = &result {
        errors.push(format!("{error:#}"));
    }
    match host.transcript() {
        Ok(observations) => evidence.outputs = observations,
        Err(error) => errors.push(format!("transcript: {error:#}")),
    }
    write_case_receipt(
        &lease.directory,
        evidence,
        errors,
        &[("host", &host_cleanup), ("cluster", &cluster_cleanup)],
    )?;
    result?;
    let host_cleanup = host_cleanup?;
    let cluster_cleanup = cluster_cleanup?;
    ensure!(
        !host_cleanup.is_empty()
            && !cluster_cleanup.is_empty()
            && host_cleanup
                .iter()
                .chain(&cluster_cleanup)
                .all(|receipt| receipt.closed),
        "S28 lifetime remains open"
    );
    let pid: u32 = std::fs::read_to_string(lease.directory.join("stdio-peer.pid"))?
        .trim()
        .parse()?;
    ensure!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "stdio MCP peer survived workbench shutdown"
    );
    println!("S28 selected=1 executed=1 MCP gates=10 reload/no-duplicate=1 cleanup=closed");
    Ok(())
}
