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
        channel: Channel::Rlm,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        cuts: Vec::new(),
        expected_terminal: "settled".into(),
        requires: vec!["FIG-4900".into()],
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
