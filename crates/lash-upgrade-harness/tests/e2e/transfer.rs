//! S13: independently read drain receipts. S12/S23/S31/S32 run on the
//! workbench product host (`e2e` handover rows).
use anyhow::{Result, ensure};
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseSpec, Channel, StoreKind},
    host::HostKind,
    provider::ProviderKind,
};

/// The catalogue is shared with H8. Final-routing claims are explicitly held
/// by the deletion units, rather than inferred from a successful business reply.
pub fn specs(store: StoreKind, artifacts: Vec<ArtifactIdentity>) -> Vec<CaseSpec> {
    vec![CaseSpec {
        id: "S13".into(),
        rules: vec!["L11".into(), "L21".into()],
        host: HostKind::UpgradeNode,
        store,
        channel: Channel::Standard,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        cuts: Vec::new(),
        expected_terminal: "settled".into(),
        // FIG-4900's landing commit is the manifest's S13 arc guard.
        requires: Vec::new(),
    }]
}

/// S13/L11: a real pinned timer keeps its generation undrained. The operator
/// floor refuses a forced removal until terminal work and all engine waits end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s13_owned_work_refuses_operator_retirement_sqlite_memory() -> Result<()> {
    use lash_upgrade_harness::node::h3::{operation_invocation, retirement, rlm};
    let (core, double) = rlm::fixture(0x493413, "await sleep(86400000); finish(42);").await?;
    let session_id = lash::SessionId::fixture("s13-pinned-work");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "upgrade-harness-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await?;
    let session = core.session(session_id.clone()).open().await?;
    let handle = session
        .send(lash::TurnInput::text("keep the old deployment pinned"))
        .id("s13-pinned-input")
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), double.server().settle()).await?;
    let run = handle
        .run()
        .await?
        .ok_or_else(|| anyhow::anyhow!("pinned input has no Run"))?;
    let invocation = operation_invocation(&double, &session_id, &run).await?;
    ensure!(
        invocation.status == "suspended",
        "work is not actually suspended: {invocation:?}"
    );
    let deployment = double
        .server()
        .pinned_deployment(&invocation.id)
        .ok_or_else(|| anyhow::anyhow!("work has no actual deployment pin"))?;
    let generation = core.build_generation().clone();
    let drain = double.stores().generation_drain();
    drain
        .mark_draining(&generation, double.server().now_ms())
        .await?;
    let refusal = retirement::retire_double(&core, &double, &generation, &deployment).await?;
    let Err(retirement::RetirementRefusal::OwnedWork { status }) = refusal else {
        anyhow::bail!("operator retired genuine pinned work: {refusal:?}");
    };
    ensure!(
        !status.drained() && status.unfinished_invocations > 0,
        "drain ignored the independently observed engine pin: {status:?}"
    );
    ensure!(
        double.server().deployments().contains(&deployment),
        "refused retirement removed the deployment"
    );
    // Abort this drain while the application finishes naturally, then ask for
    // retirement again. No readiness condition is implemented by elapsed time.
    drain.clear_draining(&generation).await?;
    let timers = double.server().timers();
    let timer = timers
        .iter()
        .filter(|timer| timer.invocation == invocation.id && timer.kind == "sleep")
        .max_by_key(|timer| timer.fire_at_ms)
        .ok_or_else(|| anyhow::anyhow!("pinned application has no sleep timer: {timers:?}"))?;
    double.server().advance_to(timer.fire_at_ms);
    let outcome =
        match tokio::time::timeout(std::time::Duration::from_secs(10), handle.outcome()).await {
            Ok(outcome) => outcome?,
            Err(error) => anyhow::bail!(
                "application timer wake failed: {error}; now={}; timers={:?}; invocations={:?}",
                double.server().now_ms(),
                double.server().timers(),
                double.server().invocations()
            ),
        };
    ensure!(
        outcome.status() == lash::TurnStatus::Answered && outcome.run() == Some(&run),
        "pinned application did not finish its original Run: {outcome:?}"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), double.server().settle()).await?;
    drain
        .mark_draining(&generation, double.server().now_ms())
        .await?;
    let status = retirement::retire_double(&core, &double, &generation, &deployment)
        .await?
        .map_err(|refusal| anyhow::anyhow!("terminal generation still refused: {refusal:?}"))?;
    ensure!(
        status.drained() && status.unfinished_invocations == 0,
        "retirement has no complete drain receipt"
    );
    ensure!(
        !double.server().deployments().contains(&deployment),
        "drained deployment remains registered"
    );
    Ok(())
}

/// S13/L11: unreadable real admin state is a typed native storage failure,
/// including on an otherwise empty marked generation; it cannot prove drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s13_unreadable_registry_fails_closed_sqlite_memory() -> Result<()> {
    let (core, double) = lash_upgrade_harness::node::h3::double_fixture(0x493414).await?;
    let generation = core.build_generation().clone();
    let stores = double.stores();
    stores
        .generation_drain()
        .mark_draining(&generation, double.server().now_ms())
        .await?;
    let registry = lash_restate::RestateDeploymentRegistry::new(
        lash_restate::RestateAdminClient::new("http://127.0.0.1:1"),
    );
    let result = lash::GenerationDrainStatus::collect(
        stores.generation_drain().as_ref(),
        stores.session_delete_ledger().as_ref(),
        |kind| stores.obligation_ledger(kind),
        &registry,
        &generation,
        double.server().now_ms(),
    )
    .await;
    ensure!(
        matches!(
            result,
            Err(lash_core::StoreError::StorageFailure {
                backend: "engine deployment registry",
                ..
            })
        ),
        "unreadable registry did not keep its typed refusal: {result:?}"
    );
    ensure!(
        !double.server().deployments().is_empty(),
        "failed query removed a deployment"
    );
    Ok(())
}

/// S13/L11/L21 on real hosts: genuine work pinned to N keeps N's deployment
/// while N+1 serves beside it. Retirement and finalize refuse typed, a
/// severed registry read refuses rather than reading drained, and once the
/// original Run finishes on N the drained deployment is removed.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries and private live Restate"]
fn s13_pinned_work_refuses_retirement_until_drained_on_upgrade_nodes() -> Result<()> {
    use crate::h3_live::{Live, command};
    use lash_upgrade_harness::harness::block_on;
    use lash_upgrade_harness::node::h3::H3Command;
    use serde_json::json;
    const SEVERED: &str = "http://127.0.0.1:1";
    let mut live = Live::setup("s13", |artifacts| {
        specs(StoreKind::SqliteFile, artifacts)
            .into_iter()
            .find(|spec| spec.id == "S13")
            .expect("S13 is catalogued")
    })?;
    let (n, next) = (live.builds.n.clone(), live.builds.next.clone());
    let session = live.case.session_id("s13");
    let view = live.case.view()?;
    let n_host = live.serve(&n, "candidate")?;
    let g_n = n_host.generation()?.to_owned();
    let n_deployment = block_on(view.deployment_at(n_host.uri()?))?.id;
    let admitted = live.h3(
        &n,
        &session,
        &H3Command::Deferred {
            key: "s13-pinned".into(),
        },
    )?;
    let run: lash_core::TurnId = serde_json::from_value(admitted["run"].clone())?;
    let (key, pinned) = live.await_suspended(&n, &session, &run)?;
    ensure!(
        pinned.pinned_deployment_id.as_deref() == Some(n_deployment.as_str()),
        "genuine work is not pinned to N's deployment: {pinned:?}"
    );
    let next_host = live.serve(&next, "successor")?;
    let g_next = next_host.generation()?.to_owned();
    let next_deployment = block_on(view.deployment_at(next_host.uri()?))?.id;
    ensure!(g_next != g_n, "N+1 serves N's generation");
    let present = |view: &lash_upgrade_harness::restate_view::RestateView| -> Result<Vec<String>> {
        Ok(block_on(view.deployments())?
            .into_iter()
            .map(|deployment| deployment.id)
            .collect())
    };
    ensure!(
        present(&view)?.contains(&n_deployment) && present(&view)?.contains(&next_deployment),
        "N and N+1 do not coexist"
    );
    let drain = |op: serde_json::Value, severed: Option<&str>| {
        command(
            json!({"action": "drain", "generation": g_n, "op": op, "severed_admin_url": severed}),
        )
    };

    live.h3(&n, &session, &drain(json!({"drain": "mark"}), None)?)?;
    let refused = live.h3(
        &n,
        &session,
        &drain(json!({"drain": "retire", "deployment": n_deployment}), None)?,
    )?;
    ensure!(
        refused["refused"] == "owned_work"
            && refused["status"]["drained"] == false
            && refused["status"]["unfinished_invocations"]
                .as_u64()
                .is_some_and(|count| count > 0),
        "operator retired genuine pinned work: {refused}"
    );
    let finalize = live.h3(&n, &session, &drain(json!({"drain": "finalize"}), None)?)?;
    ensure!(
        finalize["refused"].is_string() && finalize.get("finalized").is_none(),
        "finalize retired a generation with pinned work: {finalize}"
    );
    ensure!(
        present(&view)?.contains(&n_deployment),
        "refused retirement removed the deployment"
    );

    // The query fault: an unreadable registry is a typed native storage
    // failure and never proves drain, so nothing is removed.
    let read = live.h3(
        &n,
        &session,
        &drain(json!({"drain": "status"}), Some(SEVERED))?,
    )?;
    ensure!(
        read["error"]["kind"] == "storage_failure"
            && read["error"]["backend"] == "engine deployment registry"
            && read.get("status").is_none(),
        "unreadable registry did not keep its typed refusal: {read}"
    );
    let severed = live.h3(
        &n,
        &session,
        &drain(
            json!({"drain": "retire", "deployment": n_deployment}),
            Some(SEVERED),
        )?,
    )?;
    ensure!(
        severed["refused"] == "drain_read_failed"
            && severed["cause"]["kind"] == "storage_failure"
            && severed["cause"]["backend"] == "engine deployment registry",
        "a failed drain read did not refuse retirement typed: {severed}"
    );
    ensure!(
        present(&view)?.contains(&n_deployment),
        "failed query removed a deployment"
    );

    // Abort this drain while the application finishes naturally, then ask
    // for retirement again. Completion is the source's resolution.
    live.h3(&n, &session, &drain(json!({"drain": "clear"}), None)?)?;
    let operation = lash_core::tool_run::OperationRun::for_run_id(
        lash::SessionId::fixture(session.clone()),
        &run,
    )
    .ok_or_else(|| anyhow::anyhow!("{run} is not an operation Run"))?
    .operation_id;
    live.complete_first(&n, &session, &operation, "s13-drained")?;
    let followed = live.h3(&n, &session, &H3Command::Follow { run: run.clone() })?;
    ensure!(
        followed["output"] == json!("s13-drained"),
        "pinned application did not finish its original Run: {followed}"
    );
    let snapshot = live.h3(&n, &session, &H3Command::Snapshot { run: run.clone() })?;
    let terminal: lash_core::store::RunTerminal =
        serde_json::from_value(snapshot["terminal"].clone())?;
    ensure!(
        terminal.run == run
            && terminal.kind() == lash_core::store::RunTerminalKind::Answered
            && snapshot["unfinished"] == false,
        "original Run did not settle Answered: {snapshot}"
    );
    live.quiesce()?;
    let finished = live.run_invocations(&key)?;
    ensure!(
        finished.len() == 1
            && finished[0].id == pinned.id
            && finished[0].status == "completed"
            && finished[0].pinned_deployment_id.as_deref() == Some(n_deployment.as_str()),
        "the original journal did not finish on N: {finished:?}"
    );
    live.journal(&pinned.id, "s13-pinned", &run)?;

    live.h3(&n, &session, &drain(json!({"drain": "mark"}), None)?)?;
    let retired = live.h3(
        &n,
        &session,
        &drain(json!({"drain": "retire", "deployment": n_deployment}), None)?,
    )?;
    ensure!(
        retired["retired"]["drained"] == true && retired["retired"]["unfinished_invocations"] == 0,
        "retirement has no complete drain receipt: {retired}"
    );
    let remaining = present(&view)?;
    ensure!(
        !remaining.contains(&n_deployment) && remaining.contains(&next_deployment),
        "drained N was not removed or N+1 was: {remaining:?}"
    );
    let finalized = live.h3(&n, &session, &drain(json!({"drain": "finalize"}), None)?)?;
    ensure!(
        finalized.get("finalized").is_some(),
        "drained generation did not finalize: {finalized}"
    );
    live.stop(next_host, "successor")?;
    live.stop(n_host, "candidate")?;
    live.finish()
}
