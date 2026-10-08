//! A plugin operation on a workbench node: S17 (an operation Run is
//! recoverably explicit). The workbench's fixture task holds its body in a
//! host gate under the operation's key until the case releases it.

use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, Leg};
use serde_json::{Value, json};

use crate::support::{self, successor};
use crate::workbench::{self, Protocol};

/// The operation's key and output.
const KEY: &str = "s17-operation";

/// The keys whose bodies entered on workbench `node`.
async fn entered(case: &Case, node: &str) -> Result<Vec<Value>> {
    let bodies = case.node(node)?.get("/api/e2e/operations/bodies").await?;
    Ok(bodies.as_array().cloned().unwrap_or_default())
}

/// Wait until the operation's body entered on `node`.
async fn wait_entered(case: &Case, node: &str) -> Result<()> {
    case.until("the operation body entered", || async {
        Ok(entered(case, node)
            .await?
            .iter()
            .any(|body| body["key"] == KEY)
            .then_some(()))
    })
    .await
}

/// Follow the operation by its recorded Run ID through `node`.
async fn follow(case: &mut Case, node: &str, run: &str) -> Result<Value> {
    let session = support::session(case);
    let result = case
        .node(node)?
        .get(&format!("/api/e2e/sessions/{session}/operations/{run}"))
        .await?;
    case.evidence
        .outputs
        .push(json!({"node": node, "run": run, "result": result}));
    Ok(result)
}

case!(
    s17_operation_run_is_recoverably_explicit,
    SqliteFile,
    Live,
    s17
);
case!(
    s17_operation_run_is_recoverably_explicit_resume,
    SqliteFile,
    Resume,
    s17
);
case!(
    s17_operation_run_is_recoverably_explicit_postgresql,
    Postgresql,
    Live,
    s17
);
case!(
    s17_operation_run_is_recoverably_explicit_postgresql_resume,
    Postgresql,
    Resume,
    s17
);
case!(
    s17_operation_run_is_recoverably_explicit_live_replay,
    SqliteFile,
    Live,
    s17
);

/// The operation is admitted and its caller's handle dropped: the admission
/// answers with the Run on its own, and only the recorded Run ID reattaches
/// a follower. The follower does not answer while the body is held. On the
/// resume leg the node dies holding it; the claimer runs the uncommitted
/// task again. The follow answers the released output, and a cold
/// reattach by the same Run ID answers the same.
async fn s17(case: &mut Case) -> Result<()> {
    let options = workbench::options(case, "S01", Protocol::Standard, &[], json!({}))?;
    case.boot(Host::Workbench, "node-a", options.clone())
        .await?;
    let session = support::session(case);
    let started = case
        .node("node-a")?
        .post(
            &format!("/api/e2e/sessions/{session}/operations"),
            &json!({"key": KEY, "output": KEY}),
        )
        .await?;
    case.record_barrier(json!({"barrier": "admitted", "receipt": started}));
    ensure!(
        started["admitted"] == true,
        "the operation was not admitted: {started}"
    );
    let run = started["run"]
        .as_str()
        .context("the admission names no Run")?
        .to_owned();
    wait_entered(case, "node-a").await?;
    // The admission is the session's queued command, durable before the
    // body settles: the product's queued-work read lists it by the batch
    // the Run is named after.
    let queued = case
        .node("node-a")?
        .get(&format!("/api/queued-work?session_id={session}"))
        .await?;
    case.evidence
        .stores
        .push(json!({"at": "the body held", "queued_work": queued}));
    ensure!(
        queued
            .as_array()
            .is_some_and(|batches| batches.iter().any(|batch| batch["batch_id"]
                .as_str()
                .is_some_and(|batch| run.ends_with(batch)))),
        "the durable admission lists {queued}, not the held Run {run}"
    );
    let node = match case.leg {
        Leg::Live => "node-a",
        Leg::Resume => {
            case.kill("node-a", "the operation body held, nothing committed")
                .await?;
            let next = successor(case);
            case.boot(Host::Workbench, next, options.clone()).await?;
            wait_entered(case, next).await?;
            next
        }
    };
    // The reattached follower waits on the held body.
    let url = case.node(node)?.url.clone();
    let path = format!("{url}/api/e2e/sessions/{session}/operations/{run}");
    let pending = tokio::spawn(async move {
        reqwest::Client::new()
            .get(path)
            .send()
            .await?
            .json::<Value>()
            .await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    ensure!(
        !pending.is_finished(),
        "the follower answered before the body was released"
    );
    let released = case
        .node(node)?
        .post(&format!("/api/e2e/operations/{KEY}/release"), &json!({}))
        .await?;
    ensure!(
        released["released"] == true,
        "no gate held {KEY}: {released}"
    );
    let result = tokio::time::timeout_at(case.deadline.into(), pending)
        .await
        .context("the follower did not answer by the deadline")???;
    case.evidence
        .outputs
        .push(json!({"node": node, "run": run, "result": result}));
    ensure!(
        result["run"] == json!(run) && result["output"] == KEY,
        "the follower answered {result}"
    );
    let bodies = entered(case, node).await?;
    ensure!(
        bodies.iter().filter(|body| body["key"] == KEY).count() == 1,
        "{node} ran the body {bodies:?}"
    );
    // A cold reattach by the recorded Run ID answers the stored terminal.
    case.stop(node).await?;
    case.boot(Host::Workbench, node, options).await?;
    let queued = case
        .node(node)?
        .get(&format!("/api/queued-work?session_id={session}"))
        .await?;
    case.evidence
        .stores
        .push(json!({"at": "after the operation", "queued_work": queued}));
    ensure!(
        queued.as_array().is_some_and(Vec::is_empty),
        "the settled operation is still queued: {queued}"
    );
    let again = follow(case, node, &run).await?;
    ensure!(
        again == result,
        "a cold reattach answered {again}, not {result}"
    );
    ensure!(
        entered(case, node).await?.is_empty(),
        "a cold reattach ran the body again"
    );
    case.stop(node).await?;
    Ok(())
}
