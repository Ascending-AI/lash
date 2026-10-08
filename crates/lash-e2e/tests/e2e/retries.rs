//! Reported retries on a durable node: S06 (a retry keeps its recorded
//! schedule across a killed node) and S07 (cancelling during a backoff
//! starts nothing new).

use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use lash_e2e::{Case, Host, NodeOptions};
use serde_json::{Value, json};

use crate::support::{self, successor};

/// Tools `a` and `b`, each `Repeatable` with two attempts and a backoff of
/// `delay_ms`, whose first attempt fails with a typed, retryable failure.
/// `b`'s body is held, so its retry commits after `a`'s, in its own commit.
fn retried(case: &Case, delay_ms: u64) -> Result<NodeOptions> {
    let tool = |name: &str, value: &str, hold: bool| json!({"name": name, "value": value, "hold": hold, "fail_attempts": 1, "policy": support::repeatable(2, delay_ms)});
    Ok(NodeOptions {
        fixture: Some(case.fixture(
            json!([tool("a", "A", false), tool("b", "B", true)]),
            json!([{"calls": ["a", "b"]}, {"text": "A|B"}]),
        )?),
        ..NodeOptions::default()
    })
}

/// Wait until node-a recorded `a`'s retry, release `b`'s failing first
/// attempt, and wait until its retry is recorded too.
async fn retries_recorded(case: &Case) -> Result<()> {
    case.until("a's retry recorded", || async {
        Ok((support::applied(case, "node-a", "round.retry")? >= 1).then_some(()))
    })
    .await?;
    case.control.release("b");
    case.until("b's retry recorded", || async {
        Ok((support::applied(case, "node-a", "round.retry")? >= 2).then_some(()))
    })
    .await
}

/// The retry records' bodies, by call, from store facts.
fn schedule(facts: &Value) -> Vec<(String, Value)> {
    support::records(facts, "retry")
        .into_iter()
        .map(|record| {
            (
                record["call"].as_str().unwrap_or_default().to_owned(),
                record["record"].clone(),
            )
        })
        .collect()
}

/// The (attempt, entry time) of each body entry of `tool`.
fn attempts(case: &Case, tool: &str) -> Result<Vec<(u64, u64)>> {
    Ok(case
        .bodies()?
        .into_iter()
        .filter(|line| line["tool"] == tool)
        .map(|line| {
            (
                line["attempt"].as_u64().unwrap_or_default(),
                line["at_ms"].as_u64().unwrap_or_default(),
            )
        })
        .collect())
}

case!(s06_reported_retries_retain_schedule, SqliteFile, Live, s06);
case!(
    s06_reported_retries_retain_schedule_resume,
    SqliteFile,
    Resume,
    s06
);

/// Both first attempts fail and their retries commit with a due time; the
/// node is killed during the backoff. The claimer runs each second attempt
/// at its recorded due time and never again, from the recorded schedule.
async fn s06(case: &mut Case) -> Result<()> {
    const DELAY_MS: u64 = 4_000;
    let options = retried(case, DELAY_MS)?;
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let turn = support::submit(case, "node-a", "turn-1", "retry a and b").await?;
    retries_recorded(case).await?;
    case.kill("node-a", "both retries recorded, mid-backoff")
        .await?;
    let cut = support::record_facts(case, &turn.run, "mid-backoff").await?;
    let recorded = schedule(&cut);
    ensure!(recorded.len() == 2, "the cut holds retries {recorded:?}");
    let next = successor(case);
    case.boot(Host::Consumer, next, options).await?;
    let outcome = support::follow(case, next, &turn.input).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("A|B"),
        "the turn settled {kind} {reply:?}"
    );
    for tool in ["a", "b"] {
        let attempts = attempts(case, tool)?;
        ensure!(
            attempts
                .iter()
                .map(|(attempt, _)| *attempt)
                .collect::<Vec<_>>()
                == [1, 2],
            "{tool} ran attempts {attempts:?}, not 1 then 2 once each"
        );
        ensure!(
            attempts[1].1 >= attempts[0].1 + DELAY_MS,
            "{tool}'s second attempt ran before its backoff ended: {attempts:?}"
        );
    }
    case.stop(next).await?;
    let end = support::record_facts(case, &turn.run, "after the turn settled").await?;
    ensure!(
        end["end"]["kind"] == "Answered",
        "the store's turn end is {end}"
    );
    ensure!(
        schedule(&end) == recorded,
        "the retry schedule changed: {:?} then {recorded:?}",
        schedule(&end)
    );
    Ok(())
}

case!(
    s07_cancellation_during_backoff_starts_nothing_new,
    SqliteFile,
    Live,
    s07
);
case!(
    s07_cancellation_during_backoff_starts_nothing_new_resume,
    SqliteFile,
    Resume,
    s07
);

/// Both retries are pending when the turn is cancelled, and the node is
/// killed right after the cancel. The node that claims the turn settles it
/// cancelled, and no second attempt ever starts, past every due time.
async fn s07(case: &mut Case) -> Result<()> {
    const DELAY_MS: u64 = 3_000;
    let options = retried(case, DELAY_MS)?;
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let turn = support::submit(case, "node-a", "turn-1", "retry a and b").await?;
    retries_recorded(case).await?;
    let pending_since = Instant::now();
    support::cancel(case, "node-a", &turn.input).await?;
    case.kill("node-a", "cancel requested during the backoff")
        .await?;
    support::record_facts(case, &turn.run, "after the cancel and the kill").await?;
    let next = successor(case);
    case.boot(Host::Consumer, next, options).await?;
    let outcome = support::follow(case, next, &turn.input).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(kind == "cancelled", "the turn settled {kind}: {outcome}");
    // Every due time passes with nothing started.
    let past_due = pending_since + Duration::from_millis(DELAY_MS * 2);
    case.until("every retry's due time passed", || async {
        Ok((Instant::now() >= past_due).then_some(()))
    })
    .await?;
    for tool in ["a", "b"] {
        let attempts = attempts(case, tool)?;
        ensure!(
            attempts.len() == 1 && attempts[0].0 == 1,
            "{tool} ran {attempts:?} after its cancel"
        );
    }
    let status = case.node(next)?.get("/control/drain-status").await?;
    ensure!(
        status["in_flight_turns"] == 0 && status["remaining_invocations"] == 0,
        "work remains after the cancel: {status}"
    );
    case.stop(next).await?;
    let end = support::record_facts(case, &turn.run, "after the turn settled").await?;
    ensure!(
        end["end"]["kind"] == "Cancelled",
        "the store's turn end is {end}"
    );
    Ok(())
}
