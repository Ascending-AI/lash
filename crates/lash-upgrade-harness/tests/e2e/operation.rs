//! S17 ("operation Run is recoverably explicit", L08/L14) runs on the real agent-workbench product host.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use lash_upgrade_harness::e2e::case::CaseLease;
use lash_upgrade_harness::e2e::cluster::{ClusterControl, LocalCluster};
use lash_upgrade_harness::e2e::control::{CleanupReceipt, ProcessReceipt, WorkIdentity};
use lash_upgrade_harness::e2e::evidence::{CaseReceipt, DecodedRecord, Evidence, Verdict};
use lash_upgrade_harness::e2e::host::{HostAdapter, HostCommand};
use lash_upgrade_harness::e2e::host_adapters::workbench::WorkbenchHost;
use lash_upgrade_harness::restate_view::RestateView;
use serde_json::{Value, json};
use tokio::process::Command;

const OUTPUT: &str = "s17-exact-result";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s17_workbench_operation_drop_and_follow() -> Result<()> {
    let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
    std::fs::create_dir_all(&root)?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut lease = CaseLease::new("s17-workbench", root.join("s17-workbench"), deadline)?;
    let base: u16 = std::env::var("LASH_E2E_PORT_BASE")?.parse()?;
    ensure!(base <= u16::MAX - 50, "private port range overflow");
    lease.ports = (base + 10..base + 12).collect();
    let server = super::artifact(
        "restate-server",
        std::env::var("LASH_RESTATE_SERVER_BIN")?.into(),
    )?;
    let artifact = super::artifact(
        "agent-workbench",
        std::env::var("LASH_WORKBENCH_E2E_BIN")?.into(),
    )?;
    let mut cluster = LocalCluster::new(base, deadline);
    let mut host = WorkbenchHost::new(
        format!("http://127.0.0.1:{base}"),
        format!("http://127.0.0.1:{}", base + 1),
        base + 10,
        base + 11,
    )?
    .configure(BTreeMap::from([
        ("OPENROUTER_API_KEY".into(), "case-owned-fixture".into()),
        ("AGENT_WORKBENCH_PROTOCOL".into(), "standard".into()),
        (
            "AGENT_WORKBENCH_SEARCH_MCP_URL".into(),
            "http://127.0.0.1:1/mcp".into(),
        ),
    ]))?;
    let result: Result<Evidence> = async {
        let boot = cluster.boot(&server, 1, &mut lease).await?;
        super::write(&lease.directory.join("cluster-boot.json"), &boot)?;
        let ready = host.boot(&artifact, &mut lease).await?;
        ensure!(
            ready.protocol == 7 && ready.process.pid > 0,
            "workbench did not boot a real V7 process"
        );
        let store_root = lease.directory.join("workbench-data/lash-sessions");
        // One store set answers every store read of this case.
        let stores: Arc<dyn lash::StoreSet> =
            Arc::new(lash::sqlite::SqliteStoreSet::open(&store_root).await?);
        let created = host
            .command(HostCommand::Process {
                action: "create-session".into(),
                input: json!({"session":"s17-workbench"}),
            })
            .await?;
        let session_id = created.output["session_id"]
            .as_str()
            .context("create-session returned no actual id")?
            .to_owned();
        let session = lash::SessionId::parse(&session_id)?;
        let submitted = host
            .command(HostCommand::Operation {
                session: "s17-workbench".into(),
                input: json!({"key":"s17-input","output":OUTPUT}),
            })
            .await?;
        let run = submitted.work.run.clone();
        ensure!(!run.is_empty(), "operation returned no recorded Run ID");
        let run_typed = lash::TurnId::parse(&run)?;
        // The route dropped the caller's handle at submit; the native tool
        // body waits on the fixture gate until the case releases it.
        let receipt = loop {
            let bodies = host
                .command(HostCommand::Process {
                    action: "operation-bodies".into(),
                    input: json!({}),
                })
                .await?;
            if let Some(receipt) = bodies
                .output
                .as_array()
                .and_then(|bodies| bodies.iter().find(|receipt| receipt["key"] == OUTPUT))
            {
                break receipt.clone();
            }
            ensure!(
                Instant::now() < lease.deadline,
                "operation native tool body never entered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let call_id = receipt["call_id"]
            .as_str()
            .context("body receipt has no actual call identity")?
            .to_owned();
        let store: Arc<dyn lash::persistence::RuntimeStore> = stores.session_store_factory();
        // While the body is held, durable admission answers separately from
        // completion: the admission still owns the run, the store holds no
        // terminal, and the invocation is recorded but not completed.
        ensure!(
            store.run_executor(&session, &run_typed).await?.is_some(),
            "operation lost its durable executor admission while its tool ran"
        );
        ensure!(
            store.run_terminal(&session, &run_typed).await?.is_none(),
            "operation wrote a terminal while its tool was held"
        );
        let key = lash::restate::recorded_turn_invocation_key(
            store.as_ref(),
            &session,
            &run_typed,
        )
        .await?
        .context("operation has no retained invocation key")?;
        let view = RestateView::new(&boot.nodes[0].admin_url, &lease.namespace)?;
        let prefix = view.service_name("LashTurn");
        let rows: Vec<Value> = view
            .query(&format!(
                "SELECT id,target_service_name,pinned_service_protocol_version,status FROM sys_invocation WHERE target_service_key='{}' AND target_handler_name='run'",
                key.replace('\'', "''")
            ))
            .await?;
        let rows: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                row["target_service_name"].as_str().is_some_and(|name| {
                    name == prefix || name.starts_with(&format!("{prefix}_g"))
                })
            })
            .collect();
        ensure!(
            rows.len() == 1,
            "operation Run has no unique actual invocation: {rows:?}"
        );
        ensure!(
            rows[0]["pinned_service_protocol_version"] == 7,
            "operation invocation did not negotiate V7"
        );
        ensure!(
            rows[0]["status"] != "completed",
            "operation invocation completed while its tool was held"
        );
        let invocation = rows[0]["id"]
            .as_str()
            .context("invocation has no id")?
            .to_owned();
        host.command(HostCommand::Process {
            action: "release-operation".into(),
            input: json!({"key":OUTPUT}),
        })
        .await?;
        let first = tokio::time::timeout(
            Duration::from_secs(10),
            host.command(HostCommand::Attach { run: run.clone() }),
        )
        .await
        .context("reattach did not answer within 10 seconds")??;
        ensure!(
            first.output["output"] == OUTPUT,
            "reattached operation changed its result"
        );
        ensure!(
            first.output["run"].as_str() == Some(run.as_str()),
            "reattachment acquired another Run"
        );
        let second = host
            .command(HostCommand::Attach { run: run.clone() })
            .await?;
        ensure!(
            second.output == first.output,
            "durable operation result changed between followers"
        );
        loop {
            let admission = host
                .command(HostCommand::Process {
                    action: "admission".into(),
                    input: json!({"session_id":session_id}),
                })
                .await?;
            if admission.output["unfinished"].is_null() {
                break;
            }
            ensure!(
                Instant::now() < lease.deadline,
                "operation retained admission"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let terminal = loop {
            if let Some(terminal) = store.run_terminal(&session, &run_typed).await? {
                break terminal;
            }
            ensure!(
                Instant::now() < lease.deadline,
                "settled operation wrote no store terminal"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        ensure!(
            terminal.kind() == lash::persistence::RunTerminalKind::Answered,
            "operation stored a non-answer terminal: {terminal:?}"
        );
        loop {
            let rows: Vec<Value> = view
                .query(&format!(
                    "SELECT status FROM sys_invocation WHERE id = '{}'",
                    invocation.replace('\'', "''")
                ))
                .await?;
            if rows
                .first()
                .is_some_and(|row| row["status"] == "completed")
            {
                break;
            }
            ensure!(
                Instant::now() < lease.deadline,
                "operation invocation never completed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let work = WorkIdentity {
            ingress: run.clone(),
            run: run.clone(),
            segment: invocation.clone(),
            call: Some(call_id.clone()),
            ordinal: receipt["attempt"]
                .as_u64()
                .and_then(|attempt| u32::try_from(attempt).ok()),
        };
        let journals = view.journal(&work, &invocation, 7).await?;
        ensure!(!journals.is_empty(), "operation invocation has no journal");
        ensure!(
            journals.iter().any(|fact| matches!(
                &fact.decoded,
                Some(DecodedRecord::Attempt(entry)) if entry.call_id.to_string() == call_id
            )),
            "operation journal holds no Attempt record for the actual tool call {call_id}"
        );
        let bodies = host
            .command(HostCommand::Process {
                action: "operation-bodies".into(),
                input: json!({}),
            })
            .await?;
        let bodies = bodies
            .output
            .as_array()
            .context("operation bodies must be an array")?
            .clone();
        ensure!(
            bodies.iter().filter(|receipt| receipt["key"] == OUTPUT).count() == 1,
            "reattach re-executed the native tool"
        );
        // The browser layer follows the same Run same-origin: a page on the
        // workbench origin fetches the follow and admission routes.
        let python = PathBuf::from(std::env::var("LASH_E2E_PYTHON")?);
        let script = PathBuf::from(std::env::var("LASH_E2E_REPO")?)
            .join("scripts/e2e-workbench-operation.py");
        let out_path = lease.directory.join("s17-browser.json");
        let mut browser = Command::new(&python)
            .arg(&script)
            .arg("--base-url")
            .arg(&ready.endpoint)
            .arg("--session")
            .arg(&session_id)
            .arg("--run")
            .arg(&run)
            .arg("--out")
            .arg(&out_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(
                lease.directory.join("s17-browser.stderr"),
            )?)
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;
        let pid = browser.id().context("browser PID missing")?;
        lease.processes.push(ProcessReceipt {
            role: "workbench-browser".into(),
            pid,
            incarnation: 1,
            log: lease
                .directory
                .join("s17-browser.stderr")
                .display()
                .to_string(),
        });
        let status = match tokio::time::timeout(
            lease.deadline.saturating_duration_since(Instant::now()),
            browser.wait(),
        )
        .await
        {
            Ok(status) => status?,
            Err(_) => {
                Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .status()
                    .await?;
                browser.wait().await?
            }
        };
        lease.cleanup.push(CleanupReceipt {
            resource: format!("pid:{pid}"),
            closed: true,
            detail: "browser follow process reaped".into(),
        });
        ensure!(status.success(), "browser follow/admission oracle failed");
        let browser: Value = serde_json::from_slice(&std::fs::read(&out_path)?)?;
        ensure!(
            browser["follow"] == first.output,
            "browser follow differs from the API follow"
        );
        ensure!(
            browser["follow"]["output"] == OUTPUT,
            "browser follow lost the exact result"
        );
        ensure!(
            browser["admission"]["unfinished"].is_null(),
            "browser sees a retained admission"
        );
        let mut evidence = Evidence::empty("s17-workbench".into());
        evidence.artifacts = vec![server.clone(), artifact.clone()];
        evidence.journals = journals;
        evidence.stores.push(json!({
            "kind":"s17_run_terminal",
            "store":store_root.join("durable-core.db"),
            "record":terminal,
        }));
        evidence.effects.push(json!({"kind":"s17_body_receipts","receipts":bodies}));
        evidence.effects.extend(host.trace_records()?);
        evidence.effects.push(
            json!({"kind":"s17_browser","follow":browser["follow"],"admission":browser["admission"]}),
        );
        evidence.outputs = host.transcript()?;
        Ok(evidence)
    }
    .await;
    let mut errors = Vec::new();
    let mut evidence = match result {
        Ok(evidence) => evidence,
        Err(error) => {
            errors.push(format!("{error:#}"));
            let mut evidence = Evidence::empty("s17-workbench".into());
            evidence.outputs = host.transcript().unwrap_or_default();
            evidence
        }
    };
    evidence.artifacts = vec![server.clone(), artifact.clone()];
    let host_cleanup = host.stop().await;
    let cluster_cleanup = cluster.finish().await;
    for (resource, cleanup) in [("host", host_cleanup), ("cluster", cluster_cleanup)] {
        match cleanup {
            Ok(receipts) => evidence.cleanup.extend(receipts),
            Err(error) => {
                errors.push(format!("{resource} cleanup: {error:#}"));
                evidence.cleanup.push(CleanupReceipt {
                    resource: resource.into(),
                    closed: false,
                    detail: format!("{error:#}"),
                });
            }
        }
    }
    if evidence.cleanup.is_empty() || evidence.cleanup.iter().any(|r| !r.closed) {
        errors.push("case leaked owned resources".into());
    }
    super::write(&lease.directory.join("evidence.json"), &evidence)?;
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    let verdict = error
        .as_ref()
        .map_or(Verdict::Passed, |reason| Verdict::Failed {
            reason: reason.clone(),
        });
    let counts = CaseReceipt { evidence, verdict }.write(&lease.directory)?;
    super::write(
        &lease.directory.join("result.json"),
        &json!({"scenario":"S17","variant":"s17-workbench","selected":counts.selected,
        "executed":counts.executed,"passed":counts.passed,"failed":counts.failed,"not_run":counts.not_run,
        "error":error}),
    )?;
    if let Some(error) = error {
        bail!("{error}");
    }
    counts.reconcile()?;
    println!("S17 workbench selected=1 executed=1 passed=1 failed=0 not_run=0");
    Ok(())
}
