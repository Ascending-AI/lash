//! Completion waits on a durable node: S21 (a source's resolution is first
//! winner and immutable, and a cancelled run's wait refuses it).

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, Leg, NodeOptions};
use serde_json::{Value, json};

use crate::support::{self, successor};

/// Resolve completion `key` with `value` through node `node`, answering the
/// store's answer.
async fn resolve(case: &mut Case, node: &str, key: &str, value: Value) -> Result<String> {
    let answer = case
        .node(node)?
        .post("/completions", &json!({"key": key, "value": value}))
        .await?;
    case.evidence
        .effects
        .push(json!({"resolve": key, "value": value, "node": node, "answer": answer}));
    Ok(answer["answer"].as_str().unwrap_or_default().to_owned())
}

/// The completion key of the `nth` (from 0) deferred body entry.
async fn key(case: &Case, nth: usize) -> Result<String> {
    case.until("the source deferred", || async {
        Ok(case
            .bodies()?
            .into_iter()
            .filter(|line| line["tool"] == "source")
            .nth(nth)
            .and_then(|line| line["completion"].as_str().map(ToOwned::to_owned)))
    })
    .await
}

case!(s21_source_resolution_is_immutable, SqliteFile, Live, s21);
case!(
    s21_source_resolution_is_immutable_resume,
    SqliteFile,
    Resume,
    s21
);
case!(
    s21_source_resolution_is_immutable_postgresql,
    Postgresql,
    Live,
    s21
);
case!(
    s21_source_resolution_is_immutable_postgresql_resume,
    Postgresql,
    Resume,
    s21
);

/// A deferred source's wait exists from its minting. Its first resolution
/// wins; the same resolution again answers `AlreadyResolved`, another one
/// `Conflict`, and a key that names no wait `Unknown`. On the resume leg the
/// node that minted the wait dies first, and the resolution lands before
/// the claimer awaits it. The turn presents the winning value once. A
/// second turn's source is cancelled with its run: its resolution answers
/// `Revoked` and revives nothing.
async fn s21(case: &mut Case) -> Result<()> {
    let fixture = case.fixture(
        json!([{"name": "source", "value": null, "deferred": true}]),
        json!([{"calls": ["source"]}, {"text": "resolved"}]),
    )?;
    let options = NodeOptions {
        fixture: Some(fixture),
        ..NodeOptions::default()
    };
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let first = support::submit(case, "node-a", "turn-1", "resolve the source").await?;
    let source = key(case, 0).await?;
    // The round pinned the source's wait when it admitted the call, so the
    // wait exists before the body ran.
    let node = if case.leg == Leg::Resume {
        // The body logs its key before its park commits. A node killed in
        // between leaves a started call with no outcome, which its claimer
        // settles interrupted; this fault is a node dying with the source
        // parked.
        case.until("the source's park committed", || async {
            Ok((support::applied(case, "node-a", "round.outcome")? >= 1).then_some(()))
        })
        .await?;
        case.kill("node-a", "the source pending").await?;
        let next = successor(case);
        case.boot(Host::Consumer, next, options.clone()).await?;
        next
    } else {
        "node-a"
    };
    let won = resolve(case, node, &source, json!("winner")).await?;
    ensure!(won == "Resolved", "the first resolution answered {won}");
    let again = resolve(case, node, &source, json!("winner")).await?;
    ensure!(
        again == "AlreadyResolved",
        "the same resolution answered {again}"
    );
    let other = resolve(case, node, &source, json!("loser")).await?;
    ensure!(other == "Conflict", "another resolution answered {other}");
    let unknown = resolve(case, node, &"0".repeat(32), json!("stray")).await?;
    ensure!(
        unknown == "Unknown",
        "a key naming no wait answered {unknown}"
    );
    let outcome = support::follow(case, node, &first.input).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("resolved"),
        "the first turn settled {kind} {reply:?}"
    );
    let calls = case.model_calls()?;
    let presented = &calls.last().context("no model call")?["results"]["source-0"];
    ensure!(
        presented == "winner",
        "the model saw {presented}, not the winning value"
    );
    let second = support::submit(case, node, "turn-2", "cancel the source").await?;
    let revoked_key = key(case, 1).await?;
    ensure!(
        revoked_key != source,
        "the second turn reused the first wait"
    );
    let receipt = support::cancel(case, node, &second.input).await?;
    ensure!(
        receipt.to_string().contains("Requested"),
        "the cancel was not requested: {receipt}"
    );
    let outcome = support::follow(case, node, &second.input).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(
        kind == "cancelled",
        "the second turn settled {kind}: {outcome}"
    );
    if support::store_readable_live(case) {
        let wait = support::wait_facts(case, &revoked_key).await?;
        case.evidence
            .stores
            .push(json!({"at": "the cancelled turn's wait", "wait": wait}));
    }
    let late = resolve(case, node, &revoked_key, json!("too late")).await?;
    ensure!(
        late == "Revoked",
        "a resolution after the cancel answered {late}"
    );
    let again = support::follow(case, node, &second.input).await?;
    ensure!(
        support::settled(&again).0 == "cancelled",
        "the late resolution revived the run: {again}"
    );
    ensure!(
        case.model_calls()?.len() == calls.len() + 1,
        "the cancelled turn called the model again"
    );
    case.stop(node).await?;
    let end = support::record_facts(case, &second.run, "after both turns").await?;
    ensure!(
        end["end"]["kind"] == "Cancelled",
        "the store's second turn end is {end}"
    );
    Ok(())
}
