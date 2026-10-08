//! Several lash nodes over one PostgreSQL store: S14 (a lost node's
//! accepted work finishes on another), S15 (a partitioned node cannot make a
//! second winner) and S16 (a stale node cannot publish).

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, NodeOptions, Proxy};
use serde_json::{Value, json};

use crate::support;

/// The commit-ledger lines of node `name` (every boot of it).
fn ledger(case: &Case, name: &str) -> Result<Vec<Value>> {
    lash_e2e::read_jsonl(&case.dir.join(format!("commits-{name}.jsonl")))
}

/// The lines of `lines` under `label` that the store applied (or refused).
fn under<'a>(lines: &'a [Value], label: &str, applied: bool) -> Vec<&'a Value> {
    lines
        .iter()
        .filter(|line| line["label"] == label && line["applied"] == applied)
        .collect()
}

/// Wait until node `name`'s ledger shows `label` applied `count` times.
async fn applied(case: &Case, name: &str, label: &str, count: usize) -> Result<()> {
    case.until(&format!("{name} applied {label} x{count}"), || async {
        Ok((under(&ledger(case, name)?, label, true).len() >= count).then_some(()))
    })
    .await
}

/// A proxy between a node and the case's database, and the URL through it.
async fn proxied(case: &Case) -> Result<(Proxy, String)> {
    let mut url = reqwest::Url::parse(case.database_url()?)?;
    let host = url.host_str().context("the database URL names no host")?;
    let port = url.port().unwrap_or(5432);
    let target = tokio::net::lookup_host((host, port))
        .await?
        .next()
        .context("the database host does not resolve")?;
    let proxy = Proxy::start(target).await?;
    url.set_host(Some("127.0.0.1"))?;
    url.set_port(Some(proxy.addr().port()))
        .map_err(|()| anyhow::anyhow!("set the proxy port"))?;
    Ok((proxy, url.to_string()))
}

case!(
    s14_lost_node_work_finishes_on_another_node_postgresql,
    Postgresql,
    Live,
    s14
);

/// Node A owns the session: one tool's outcome is durable, a `Once` body and
/// a `Repeatable` body are running. A is SIGKILLed; node B, already up over
/// the same database, reaps it and claims the session. The durable outcome
/// is reused, the `Once` call records `Interrupted` without running again,
/// the `Repeatable` one reruns at its attempt, and the accepted turn settles
/// once on B.
async fn s14(case: &mut Case) -> Result<()> {
    let fixture = case.fixture(
        json!([
            {"name": "done", "value": "D"},
            {"name": "once", "value": "O", "hold": true},
            {"name": "again", "value": "R", "hold": true, "policy": support::repeatable(2, 0)},
        ]),
        json!([{"calls": ["done", "once", "again"]}, {"text": "settled"}]),
    )?;
    let options = NodeOptions {
        fixture: Some(fixture),
        ..NodeOptions::default()
    };
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let turn = support::submit(case, "node-a", "turn-1", "three tools").await?;
    case.control.wait_held("once", 1, case.deadline).await?;
    case.control.wait_held("again", 1, case.deadline).await?;
    applied(case, "node-a", "round.outcome", 1).await?;
    case.boot(Host::Consumer, "node-b", options).await?;
    case.kill("node-a", "mid-tool: done durable, once and again running")
        .await?;
    let cut = support::record_facts(case, &turn.run, "after the kill").await?;
    ensure!(
        support::outcome_epochs(&cut).len() == 1,
        "the cut is not one durable outcome: {cut}"
    );
    case.control.release("once");
    case.control.release("again");
    let outcome = support::follow(case, "node-b", &turn.input).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("settled"),
        "the turn settled {kind} {reply:?}"
    );
    let b = ledger(case, "node-b")?;
    ensure!(
        b.iter()
            .any(|line| line["label"] == "reap" && line["from"] == "node-a"),
        "node-b did not reap node-a"
    );
    ensure!(
        support::entries(case, "done")?.len() == 1,
        "the durable body ran again"
    );
    ensure!(
        support::entries(case, "once")?.len() == 1,
        "the Once body ran again"
    );
    let again = support::entries(case, "again")?;
    ensure!(
        again.len() == 2 && again[0] == again[1],
        "the Repeatable body ran {again:?}"
    );
    let calls = case.model_calls()?;
    let presented = &calls.last().context("no model call")?["results"];
    ensure!(
        presented["done-0"] == "D" && presented["again-0"] == "R",
        "the model saw {presented}"
    );
    let once = presented["once-0"].as_str().unwrap_or_default();
    ensure!(
        once.to_lowercase().contains("interrupt"),
        "the Once call presented {once:?}, not Interrupted"
    );
    let end = support::record_facts(case, &turn.run, "after the turn settled").await?;
    ensure!(
        end["end"]["kind"] == "Answered" && support::outcome_epochs(&end).len() == 3,
        "the store's turn end is {end}"
    );
    case.stop("node-b").await?;
    Ok(())
}

case!(
    s15_partitioned_node_cannot_create_second_winner_postgresql,
    Postgresql,
    Live,
    s15
);

/// Node A runs a held tool and is partitioned from the database; a cancel
/// arrives through node B. A's body then finishes behind the partition. A
/// stops itself before its lease can be reaped; B reaps it, claims the
/// session and settles the turn cancelled. After the heal, nothing A wrote
/// behind the partition is visible and the store holds one terminal.
async fn s15(case: &mut Case) -> Result<()> {
    let fixture = case.fixture(
        json!([{"name": "work", "value": "W", "hold": true, "policy": support::repeatable(2, 0)}]),
        json!([{"calls": ["work"]}, {"text": "worked"}]),
    )?;
    let (proxy, through) = proxied(case).await?;
    case.boot(
        Host::Consumer,
        "node-a",
        NodeOptions {
            fixture: Some(fixture.clone()),
            database_url: Some(through),
            ..NodeOptions::default()
        },
    )
    .await?;
    let turn = support::submit(case, "node-a", "turn-1", "work").await?;
    case.control.wait_held("work", 1, case.deadline).await?;
    ensure!(
        under(&ledger(case, "node-a")?, "claim", true).len() == 1,
        "node-a does not own the session"
    );
    case.boot(
        Host::Consumer,
        "node-b",
        NodeOptions {
            fixture: Some(fixture),
            ..NodeOptions::default()
        },
    )
    .await?;
    proxy.partition();
    case.evidence
        .faults
        .push(json!({"fault": "partition", "node": "node-a"}));
    let receipt = support::cancel(case, "node-b", &turn.input).await?;
    ensure!(
        receipt.to_string().contains("Requested"),
        "the cancel was not requested: {receipt}"
    );
    // A's body finishes behind the partition, racing the cancel.
    case.control.release("work");
    applied(case, "node-b", "reap", 1).await?;
    let outcome = support::follow(case, "node-b", &turn.input).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(kind == "cancelled", "the turn settled {kind}: {outcome}");
    proxy.heal();
    case.evidence
        .faults
        .push(json!({"fault": "heal", "node": "node-a"}));
    let a = ledger(case, "node-a")?;
    let reaped = ledger(case, "node-b")?
        .into_iter()
        .filter(|line| line["label"] == "reap")
        .filter_map(|line| line["epoch"].as_i64())
        .max()
        .context("node-b reaped nothing")?;
    let stale: Vec<_> = a
        .iter()
        .filter(|line| {
            line["applied"] == true && line["epoch"].as_i64().is_some_and(|epoch| epoch >= reaped)
        })
        .collect();
    ensure!(
        stale.is_empty(),
        "node-a committed under the reaped epoch: {stale:?}"
    );
    let end = support::record_facts(case, &turn.run, "after the heal").await?;
    ensure!(
        end["end"]["kind"] == "Cancelled"
            && support::outcome_epochs(&end)
                .values()
                .all(|epoch| *epoch >= reaped),
        "the store holds A's writes or another terminal: {end}"
    );
    case.stop("node-b").await?;
    Ok(())
}

case!(
    s16_stale_postgresql_host_cannot_publish_terminal_publication_postgresql,
    Postgresql,
    Live,
    s16_terminal
);
case!(
    s16_stale_postgresql_host_cannot_publish_sigkill_redrive_postgresql,
    Postgresql,
    Live,
    s16_sigkill
);
case!(
    s16_stale_postgresql_host_cannot_publish_terminal_publication_postgresql_resume,
    Postgresql,
    Resume,
    s16_terminal
);
case!(
    s16_stale_postgresql_host_cannot_publish_sigkill_redrive_postgresql_resume,
    Postgresql,
    Resume,
    s16_sigkill
);

/// Boot node-a holding its turn's publication (`turn.commit`) and wait
/// until it holds there.
async fn held_at_publication(case: &mut Case) -> Result<(NodeOptions, support::Turn)> {
    let fixture = case.fixture(json!([]), json!([{"text": "published"}]))?;
    let options = NodeOptions {
        fixture: Some(fixture),
        ..NodeOptions::default()
    };
    let held = NodeOptions {
        cuts: vec![json!({"label": "turn.commit", "before": true})],
        ..options.clone()
    };
    case.boot(Host::Consumer, "node-a", held).await?;
    let turn = support::submit(case, "node-a", "turn-1", "publish").await?;
    case.held("node-a", "turn.commit").await?;
    Ok((options, turn))
}

/// The turn settles once, by its successor's publication alone.
async fn published_once(case: &mut Case, successor: &str, turn: &support::Turn) -> Result<()> {
    let outcome = support::follow(case, successor, &turn.input).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("published"),
        "the turn settled {kind} {reply:?}"
    );
    let mut published = Vec::new();
    for name in ["node-a", "node-b"] {
        published.extend(
            under(&ledger(case, name)?, "turn.commit", true)
                .into_iter()
                .cloned(),
        );
    }
    ensure!(published.len() == 1, "the turn was published {published:?}");
    let end = support::record_facts(case, &turn.run, "after the turn settled").await?;
    ensure!(
        end["end"]["kind"] == "Answered" && end["end"]["head_revision"] == 1,
        "the store's turn end is {end}"
    );
    let models = case.model_calls()?;
    ensure!(
        models.len() <= 2,
        "the model was called {} times",
        models.len()
    );
    Ok(())
}

/// The old boot is held at publication when its successor fences it: on
/// the live leg a new boot of its name, which fences every earlier boot at
/// registration; on the resume leg another node, after the old one is
/// frozen until its lease lapses and node-b reaps it, then killed frozen.
/// Live, the released stale publication is refused; either way the
/// successor publishes once.
async fn s16_terminal(case: &mut Case) -> Result<()> {
    let (options, turn) = held_at_publication(case).await?;
    let old = case.node("node-a")?.pid();
    let successor = match case.leg {
        lash_e2e::Leg::Live => {
            let index = case.boot(Host::Consumer, "node-a", options).await?;
            let fenced = case.boot_at(index)?.pid();
            case.until("the new boot registered", || async {
                Ok(ledger(case, "node-a")?
                    .iter()
                    .any(|line| line["label"] == "node.register" && line["boot"] == fenced)
                    .then_some(()))
            })
            .await?;
            // The old boot still serves until its next heartbeat finds it
            // fenced: release its held publication now.
            let stale = case
                .boot_at(index - 1)?
                .post("/control/cuts/release", &json!("turn.commit"))
                .await?;
            case.evidence
                .faults
                .push(json!({"fault": "stale publication released", "boot": old, "answer": stale}));
            case.until("the stale publication answered", || async {
                Ok(ledger(case, "node-a")?
                    .into_iter()
                    .find(|line| line["label"] == "turn.commit" && line["boot"] == old))
            })
            .await
            .and_then(|line| {
                ensure!(
                    line["applied"] == false
                        && line["error"]
                            .as_str()
                            .is_some_and(|error| error.contains("OwnershipLost")),
                    "the stale publication was not refused by the fence: {line}"
                );
                Ok(())
            })?;
            "node-a"
        }
        lash_e2e::Leg::Resume => {
            // Partitioning node-a's own connections needs it behind a proxy
            // from boot; this leg fences it by stopping its process instead,
            // so its lease lapses without a release, and node-b reaps it.
            case.node("node-a")?.freeze()?;
            case.evidence
                .faults
                .push(json!({"fault": "SIGSTOP", "node": "node-a", "boot": old}));
            case.boot(Host::Consumer, "node-b", options).await?;
            applied(case, "node-b", "reap", 1).await?;
            applied(case, "node-b", "turn.commit", 1).await?;
            // The reaped node never wakes: it dies frozen, its held
            // publication unsent.
            case.kill("node-a", "frozen and reaped, its publication held")
                .await?;
            "node-b"
        }
    };
    published_once(case, successor, &turn).await?;
    case.stop(successor).await?;
    Ok(())
}

/// The old boot dies held at publication; its successor (a new boot of its
/// name, or another node) redrives the turn and publishes it once.
async fn s16_sigkill(case: &mut Case) -> Result<()> {
    let (options, turn) = held_at_publication(case).await?;
    case.kill("node-a", "held at turn.commit").await?;
    let successor = support::successor(case);
    case.boot(Host::Consumer, successor, options).await?;
    published_once(case, successor, &turn).await?;
    case.stop(successor).await?;
    Ok(())
}
