//! Work moving between workbench nodes by drain: S12 (a deferred source
//! survives the removal of its node). A draining node claims nothing more,
//! releases each actor it owns `ready` at its next committed phase, and
//! then its lease (ADR 0106 §1); another node claims what it released.

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, Leg};
use serde_json::{Value, json};

use crate::support::{self, successor};
use crate::workbench::{self, Protocol};

/// Drain workbench `node`, answering its report once it drained.
pub async fn drain(case: &mut Case, node: &str) -> Result<Value> {
    let report = case
        .node(node)?
        .post("/api/e2e/control/drain", &json!({}))
        .await?;
    case.evidence
        .faults
        .push(json!({"fault": "drain", "node": node, "report": report}));
    ensure!(
        report["drained"].is_string(),
        "{node} did not drain: {report}"
    );
    Ok(report)
}

/// Resolve completion `key` with `value` through workbench `node`,
/// answering the store's answer.
pub async fn resolve(case: &mut Case, node: &str, key: &str, value: Value) -> Result<String> {
    let answer = case
        .node(node)?
        .post("/api/e2e/completions", &json!({"key": key, "value": value}))
        .await?;
    case.evidence
        .effects
        .push(json!({"resolve": key, "value": value, "node": node, "answer": answer}));
    Ok(answer.as_str().unwrap_or_default().to_owned())
}

/// The completion key of the first delivery of deferred body `label`.
pub async fn deferred_key(case: &Case, label: &str) -> Result<String> {
    case.until("the source deferred", || async {
        Ok(workbench::deliveries(case, label)?
            .first()
            .and_then(|line| line["completion"].as_str().map(ToOwned::to_owned)))
    })
    .await
}

/// The ledger lines of `node` under `label`.
fn ledger_lines(case: &Case, node: &str, label: &str) -> Result<Vec<Value>> {
    Ok(
        lash_e2e::read_jsonl(&case.dir.join(format!("commits-{node}.jsonl")))?
            .into_iter()
            .filter(|line| line["label"] == label)
            .collect(),
    )
}

case!(s12_deferred_survives_removal_of_n, SqliteFile, Live, s12);
case!(
    s12_deferred_survives_removal_of_n_resume,
    SqliteFile,
    Resume,
    s12
);
case!(
    s12_deferred_survives_removal_of_n_postgresql,
    Postgresql,
    Live,
    s12
);
case!(
    s12_deferred_survives_removal_of_n_postgresql_resume,
    Postgresql,
    Resume,
    s12
);

/// A code mode cell awaits a deferred source after an inline gate, and a source
/// process awaits its own pinned key. Their node N drains while both are
/// unresolved and is retired by an orderly
/// stop: it released the session `ready` and then its lease. N+1 claims
/// the session. Live, N+1 is N's name booted again and the source is
/// resolved through it; resume, N+1 is another node, and the source is
/// resolved through the drained N before N is retired, since a drained
/// core still admits work. Each source seals once and N+1 returns its
/// exact value; no body runs again.
async fn s12(case: &mut Case) -> Result<()> {
    let options = workbench::options(case, "S12", Protocol::Rlm, &[], json!({}))?;
    case.boot(Host::Workbench, "node-a", options.clone())
        .await?;
    let turn = workbench::send(case, "node-a", "S12 await the source").await?;
    let key = deferred_key(case, "source").await?;
    let session = support::session(case);
    let started = case
        .node("node-a")?
        .post(
            &format!("/api/e2e/sessions/{session}/sources/s12"),
            &json!({}),
        )
        .await?;
    let process = started["process_id"]
        .as_str()
        .context("the source process has no id")?
        .to_owned();
    let process_key: String = case
        .until("the process pinned its source", || async {
            Ok(case
                .node("node-a")?
                .get(&format!("/api/e2e/sources/{process}/key"))
                .await?
                .as_str()
                .map(ToOwned::to_owned))
        })
        .await?;
    drain(case, "node-a").await?;
    // The session parked on the source's wait: the drain releases it
    // waiting, which the resolution wakes for whoever claims it next.
    let session_actor = format!("s/{}", support::session(case));
    let ledger = lash_e2e::read_jsonl(&case.dir.join("commits-node-a.jsonl"))?;
    let drained = ledger
        .iter()
        .position(|line| line["label"] == "node.drain")
        .context("node-a recorded no drain")?;
    ensure!(
        ledger[drained..].iter().any(|line| {
            matches!(
                line["label"].as_str(),
                Some("drain.release" | "session.release")
            ) && line["actor"] == session_actor
                && line["applied"] == true
        }),
        "node-a did not release the session after its drain began: {ledger:?}"
    );
    let next = successor(case);
    // The waiting process goes back with the node's lease; the successor's
    // claim below shows it was released.
    let process_actor = format!("p/{process}");
    let won = match case.leg {
        Leg::Live => {
            case.stop("node-a").await?;
            case.boot(Host::Workbench, next, options).await?;
            [
                resolve(case, next, &key, json!("late")).await?,
                resolve(case, next, &process_key, json!("process late")).await?,
            ]
        }
        Leg::Resume => {
            // The drained node still serves the resolutions, then dies.
            case.boot(Host::Workbench, next, options).await?;
            let won = [
                resolve(case, "node-a", &key, json!("late")).await?,
                resolve(case, "node-a", &process_key, json!("process late")).await?,
            ];
            case.kill("node-a", "drained, both sources resolved through it")
                .await?;
            won
        }
    };
    ensure!(
        won == ["Resolved", "Resolved"],
        "the resolutions answered {won:?}"
    );
    let ended = case
        .node(next)?
        .get(&format!("/api/work/{process}/await"))
        .await?;
    case.evidence
        .outputs
        .push(json!({"node": next, "process": process, "await": ended}));
    ensure!(
        ended["outcome"].to_string().contains("process late"),
        "the source process ended {ended}, not with its resolved value"
    );
    let again = resolve(case, next, &process_key, json!("process late")).await?;
    ensure!(
        again == "AlreadyResolved",
        "the sealed process source answered {again}"
    );

    let outcome = workbench::follow(case, next, &turn).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("gate|late"),
        "the turn settled {kind} {reply:?}: {outcome}"
    );
    let again = resolve(case, next, &key, json!("late")).await?;
    ensure!(
        again == "AlreadyResolved",
        "the sealed source answered {again}"
    );
    for label in ["gate", "source"] {
        let runs = workbench::deliveries(case, label)?.len();
        ensure!(runs == 1, "{label}'s body ran {runs} times");
    }
    ensure!(
        workbench::stages(case)? == ["initial"],
        "the provider was asked again: {:?}",
        workbench::stages(case)?
    );
    let claims = ledger_lines(case, next, "claim")?;
    let releases = ledger_lines(case, "node-a", "node.release")?;
    ensure!(
        !releases.is_empty()
            && [&session_actor, &process_actor]
                .iter()
                .all(|actor| claims.iter().any(|line| line["actor"] == json!(actor))),
        "node-a released its lease {releases:?} and {next} claimed {claims:?}"
    );
    case.stop(next).await?;
    let wait = support::wait_facts(case, &key).await?;
    case.evidence
        .stores
        .push(json!({"at": "after the turn", "source wait": wait}));
    ensure!(
        wait["lifecycle"]
            .as_str()
            .context("no wait row")?
            .starts_with("Resolved"),
        "the source wait is {wait}"
    );
    let facts = support::record_facts(case, &turn.run, "after the turn").await?;
    ensure!(
        facts["end"]["kind"] == "Answered",
        "the store's turn end is {facts}"
    );
    Ok(())
}

/// Which loser an S11 variant races.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loser {
    /// Its body runs inline and is held.
    Inline,
    /// Its body defers to a completion.
    Deferred,
}

case!(
    s11_winner_progresses_while_loser_stays_live_inline,
    SqliteFile,
    Live,
    s11_inline
);
case!(
    s11_winner_progresses_while_loser_stays_live_deferred,
    SqliteFile,
    Live,
    s11_deferred
);
case!(
    s11_winner_progresses_while_loser_stays_live_inline_resume,
    SqliteFile,
    Resume,
    s11_inline
);
case!(
    s11_winner_progresses_while_loser_stays_live_deferred_resume,
    SqliteFile,
    Resume,
    s11_deferred
);
case!(
    s11_winner_progresses_while_loser_stays_live_inline_postgresql,
    Postgresql,
    Live,
    s11_inline
);
case!(
    s11_winner_progresses_while_loser_stays_live_deferred_postgresql,
    Postgresql,
    Live,
    s11_deferred
);
case!(
    s11_winner_progresses_while_loser_stays_live_inline_postgresql_resume,
    Postgresql,
    Resume,
    s11_inline
);
case!(
    s11_winner_progresses_while_loser_stays_live_deferred_postgresql_resume,
    Postgresql,
    Resume,
    s11_deferred
);

async fn s11_inline(case: &mut Case) -> Result<()> {
    s11(case, Loser::Inline).await
}

async fn s11_deferred(case: &mut Case) -> Result<()> {
    s11(case, Loser::Deferred).await
}

/// A code mode cell races a winner against a loser, then calls `after`. The
/// winner wins; `after` runs while the loser is still live, so the race's
/// winner progresses without the loser being cancelled for losing. The
/// work then moves while the loser is outstanding: live, its node drains;
/// resume, the node is killed and another claims the session from its
/// committed rows. An inline loser's body is held: the drain does not
/// release the session before the loser's outcome commits, and a killed
/// node's loser is recorded interrupted, its body never run again. A
/// deferred loser's wait outlives the race and the move: the turn's commit
/// is held on the node that ends it, and the loser's resolution is
/// accepted there. The cell finishes with the winner's value, and no body
/// runs twice.
async fn s11(case: &mut Case, loser: Loser) -> Result<()> {
    let (holds, extra): (&[&str], _) = match loser {
        Loser::Inline => (&["loser"], json!({})),
        Loser::Deferred => (&[], json!({"deferred_loser": true})),
    };
    let options = workbench::options(case, "S11", Protocol::Rlm, holds, extra)?;
    // The cut that holds node-a once `after` committed: its outcome, or,
    // for a deferred loser, the turn's commit it reaches next.
    let cut = match loser {
        Loser::Inline => json!({"label": "round.outcome", "nth": 2}),
        Loser::Deferred => json!({"label": "turn.commit", "before": true}),
    };
    let label = cut["label"].as_str().context("cut label")?.to_owned();
    let cut_options = lash_e2e::NodeOptions {
        cuts: vec![cut.clone()],
        ..options.clone()
    };
    let first = match (case.leg, loser) {
        (Leg::Live, Loser::Inline) => options.clone(),
        _ => cut_options.clone(),
    };
    case.boot(Host::Workbench, "node-a", first).await?;
    let turn = workbench::send(case, "node-a", "S11 race the loser").await?;
    let loser_key = match loser {
        Loser::Inline => {
            case.control.wait_held("loser", 1, case.deadline).await?;
            None
        }
        Loser::Deferred => Some(deferred_key(case, "loser").await?),
    };
    case.until("after ran", || async {
        Ok((!workbench::deliveries(case, "after")?.is_empty()).then_some(()))
    })
    .await?;
    let next = successor(case);
    match case.leg {
        Leg::Live => {
            if loser == Loser::Deferred {
                case.held("node-a", &label).await?;
            }
            let url = case.node("node-a")?.url.clone();
            let draining = tokio::spawn(async move {
                reqwest::Client::new()
                    .post(format!("{url}/api/e2e/control/drain"))
                    .json(&json!({}))
                    .send()
                    .await?
                    .json::<Value>()
                    .await
            });
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            ensure!(
                !draining.is_finished(),
                "the drain finished while the loser was outstanding"
            );
            match &loser_key {
                None => case.control.release("loser"),
                Some(key) => {
                    let answer = resolve(case, "node-a", key, json!("too late")).await?;
                    ensure!(
                        answer == "Resolved",
                        "the live loser's resolution answered {answer}"
                    );
                    workbench::release_cut(case, "node-a", &label).await?;
                }
            }
            let report = tokio::time::timeout_at(case.deadline.into(), draining)
                .await
                .context("the drain did not finish by the deadline")???;
            case.evidence
                .faults
                .push(json!({"fault": "drain", "node": "node-a", "report": report}));
            ensure!(
                report["drained"].is_string(),
                "node-a did not drain: {report}"
            );
            if loser == Loser::Inline {
                let ledger = lash_e2e::read_jsonl(&case.dir.join("commits-node-a.jsonl"))?;
                let session_actor = format!("s/{}", support::session(case));
                let released = ledger
                    .iter()
                    .rposition(|line| {
                        line["actor"] == session_actor
                            && line["label"]
                                .as_str()
                                .is_some_and(|label| label.ends_with("release"))
                    })
                    .context("node-a never released the session")?;
                ensure!(
                    ledger[..released]
                        .iter()
                        .filter(|line| line["label"] == "round.outcome")
                        .count()
                        >= 3,
                    "node-a released the session before the loser's outcome committed: {ledger:?}"
                );
            }
            case.stop("node-a").await?;
            case.boot(Host::Workbench, next, options).await?;
        }
        Leg::Resume => {
            case.held("node-a", &label).await?;
            case.kill("node-a", "after committed, the loser outstanding")
                .await?;
            case.control.release("loser");
            support::record_facts(case, &turn.run, "after the kill").await?;
            // A cell keeps its calls in its own rows: the commit ledger
            // shows the winner's and after's outcomes applied. An inline
            // loser's outcome is cut after them; a deferred loser's deferral
            // commits with the winner's outcome or on its own, as the round
            // batches them, and its wait stays pending.
            let outcomes = support::applied(case, "node-a", "round.outcome")?;
            match loser {
                Loser::Inline => ensure!(
                    outcomes == 2,
                    "node-a applied {outcomes} outcomes, not the winner's and after's"
                ),
                Loser::Deferred => {
                    ensure!(
                        outcomes >= 2,
                        "node-a applied {outcomes} outcomes, not the winner's and after's"
                    );
                    let pending = support::pending_waits(case).await?;
                    case.evidence
                        .stores
                        .push(json!({"at": "after the kill", "pending waits": pending}));
                    ensure!(
                        !pending.is_empty(),
                        "the deferred loser's wait did not survive the kill"
                    );
                }
            }
            match &loser_key {
                None => {
                    case.boot(Host::Workbench, next, options).await?;
                }
                Some(key) => {
                    case.boot(Host::Workbench, next, cut_options).await?;
                    case.held(next, &label).await?;
                    let answer = resolve(case, next, key, json!("too late")).await?;
                    ensure!(
                        answer == "Resolved",
                        "the moved loser's resolution answered {answer}"
                    );
                    workbench::release_cut(case, next, &label).await?;
                }
            }
        }
    }
    let outcome = workbench::follow(case, next, &turn).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("winner"),
        "the turn settled {kind} {reply:?}: {outcome}"
    );
    for label in ["winner", "loser", "after"] {
        let runs = workbench::deliveries(case, label)?.len();
        ensure!(runs == 1, "{label}'s body ran {runs} times");
    }
    ensure!(
        workbench::stages(case)? == ["initial"],
        "the provider was asked again: {:?}",
        workbench::stages(case)?
    );
    case.stop(next).await?;
    let facts = support::record_facts(case, &turn.run, "after the turn").await?;
    ensure!(
        facts["end"]["kind"] == "Answered",
        "the store's turn end is {facts}"
    );
    Ok(())
}
