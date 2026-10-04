//! S17–S21: operation and process lifecycles (L07/L08/L12/L14).
use anyhow::{Result, ensure};
use lash_upgrade_harness::node::h3::double_fixture;

/// L08/L14: admission is durable independently of the dropped caller; a
/// reattached follower sees the explicit operation result after its tool settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s17_operation_drop_and_follow_sqlite_memory() -> Result<()> {
    let (core, double) = double_fixture(0x493417).await?;
    core.session("s17-operation")
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "upgrade-harness-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await?;
    let session = core.session("s17-operation").open().await?;
    let operation = session
        .plugin_operations()
        .start_task_raw(
            "e2e.h3.operation",
            serde_json::json!("s17-exact-result"),
            "s17-input",
        )
        .await?;
    let run = operation.run().clone();
    drop(operation);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        session.durable().run(run.clone()).result(),
    )
    .await??;
    ensure!(
        result.output == serde_json::json!("s17-exact-result"),
        "operation result changed"
    );
    ensure!(
        session.durable().unfinished_run().await?.is_none(),
        "operation retained admission"
    );
    let invocation = double
        .server()
        .invocations()
        .into_iter()
        .find(|invocation| {
            invocation.target.contains("LashTurn")
                && invocation.target.contains(run.as_str())
                && invocation.target.ends_with("/run")
        })
        .ok_or_else(|| anyhow::anyhow!("no operation journal"))?;
    let journal = double
        .server()
        .journal(&invocation.id)
        .ok_or_else(|| anyhow::anyhow!("journal missing"))?;
    ensure!(!journal.is_empty(), "operation has no durable records");
    Ok(())
}
