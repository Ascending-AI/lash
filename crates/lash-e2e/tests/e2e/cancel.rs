//! Cancelling suspended work on a workbench node: S18 (a cancel wakes a
//! suspended await).

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host};
use serde_json::json;

use crate::support;
use crate::workbench::{self, Protocol};

case!(
    s18_cancel_wakes_suspended_process_await_process_await_cancel,
    SqliteFile,
    Live,
    s18
);
case!(
    s18_cancel_wakes_suspended_process_await_live_replay,
    SqliteFile,
    Live,
    s18
);

/// A host process sleeps for ten minutes on its durable timer, and an RLM
/// cell awaits it with `processes.await`. Once the session actor released
/// itself while the process is still running, the turn is cancelled through
/// the host's public cancel. The terminal cancellation lands long before
/// the timer, a product observer that attaches only afterwards still sees
/// it, and the session holds no wait and no unfinished turn after it.
async fn s18(case: &mut Case) -> Result<()> {
    let options = workbench::options(case, "S18", Protocol::Rlm, &[], json!({}))?;
    case.boot(Host::Workbench, "node-a", options).await?;
    let session = support::session(case);
    let sleeper = case
        .node("node-a")?
        .post(
            &format!("/api/e2e/sessions/{session}/sleepers/600000"),
            &json!({}),
        )
        .await?;
    case.record_barrier(json!({"barrier": "sleeper started", "receipt": sleeper}));
    let sleeper_id = sleeper["process_id"]
        .as_str()
        .context("the sleeper receipt names no process")?;
    let turn = workbench::send(case, "node-a", "S18 await the sleeper").await?;
    let session_actor = format!("s/{}", support::session(case));
    let suspended = case
        .until(
            "the session released awaiting the sleeping process",
            || async {
                // The runtime-wide snapshot takes no session admission while its
                // turn is awaiting; identify our sleeper by its start receipt.
                let work = case.node("node-a")?.get("/api/work").await?;
                // A durable timer leaves the sleeper's lifecycle running; waiting
                // describes a deferred tool call. The session's release below is
                // the evidence that its process await is suspended.
                let sleeping = work.as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["process"]["process_id"] == sleeper_id
                            && item["process"]["lifecycle"] == "running"
                            && item["process"]["terminal"] == false
                    })
                });
                let ledger = lash_e2e::read_jsonl(&case.dir.join("commits-node-a.jsonl"))?;
                let admitted = ledger.iter().position(|line| {
                    line["label"] == "turn.admit"
                        && line["actor"] == session_actor
                        && line["applied"] == true
                });
                let released = admitted.is_some_and(|admitted| {
                    ledger[admitted..].iter().any(|line| {
                        line["label"] == "session.release"
                            && line["actor"] == session_actor
                            && line["applied"] == true
                    })
                });
                Ok((sleeping && released).then_some(work))
            },
        )
        .await?;
    case.record_barrier(json!({"barrier": "process await suspended", "work": suspended}));
    let cancelled_at = Instant::now();
    let receipt = workbench::cancel(case, "node-a").await?;
    ensure!(
        receipt.to_string().contains(&turn.run),
        "the cancel named no running turn: {receipt}"
    );
    let outcome = workbench::follow(case, "node-a", &turn).await?;
    let waited = cancelled_at.elapsed();
    let (kind, _) = support::settled(&outcome);
    ensure!(kind == "cancelled", "the turn settled {kind}: {outcome}");
    ensure!(
        waited < Duration::from_secs(60),
        "the cancellation took {waited:?}, as if it waited on the timer"
    );
    if case.live_replay {
        ensure!(
            outcome["gaps"].as_array().is_none_or(Vec::is_empty),
            "the shared live replay store reported gaps: {outcome}"
        );
    }
    let run = turn.run.clone();
    // The turn's terminal on the product stream: its `done` item.
    let events =
        workbench::product_events(case, "node-a", |line| workbench::done(line, &run)).await?;
    case.evidence
        .outputs
        .push(json!({"late product events": events.len(), "terminal": events.last()}));
    let state = case
        .until("the cancelled turn left the active set", || async {
            let state = case
                .node("node-a")?
                .get(&format!("/api/state?session_id={session}"))
                .await?;
            Ok(state["active_turns"]
                .as_array()
                .is_some_and(Vec::is_empty)
                .then_some(state))
        })
        .await?;
    case.evidence
        .stores
        .push(json!({"at": "after the cancel", "active turns": state["active_turns"]}));
    ensure!(
        workbench::stages(case)? == ["initial"],
        "the provider was asked again: {:?}",
        workbench::stages(case)?
    );
    case.stop("node-a").await?;
    let facts = support::record_facts(case, &turn.run, "after the cancel").await?;
    ensure!(
        facts["end"]["kind"] == "Cancelled" && facts["unfinished"].is_null(),
        "the store's turn end is {facts}"
    );
    let pending = support::pending_waits(case).await?;
    case.evidence
        .stores
        .push(json!({"at": "after the cancel", "pending waits": pending}));
    ensure!(
        pending.is_empty(),
        "the cancelled turn left outstanding session waits: {pending:?}"
    );
    Ok(())
}
