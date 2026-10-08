//! Tool bodies on a durable node: S02 (a durable outcome survives a killed
//! host), S03 (an ambiguous side effect dedups at its recipient), S05
//! (opposite-order parallel results) and S30 (the external consumer's
//! accept/follow/cancel contract).

use anyhow::{Result, ensure};
use lash_e2e::{Case, Host, Leg, NodeOptions};
use serde_json::json;

use crate::support::{self, successor};

case!(s02_durable_x_survives_host_sigkill, SqliteFile, Live, s02);
case!(
    s02_durable_x_survives_host_sigkill_resume,
    SqliteFile,
    Resume,
    s02
);
case!(
    s02_durable_x_survives_host_sigkill_postgresql,
    Postgresql,
    Live,
    s02
);
case!(
    s02_durable_x_survives_host_sigkill_postgresql_resume,
    Postgresql,
    Resume,
    s02
);
case!(
    s02_durable_x_survives_host_sigkill_live_replay,
    SqliteFile,
    Live,
    s02
);

/// Tools `a` (Once) and `b` (Repeatable) run in one round. `a` finishes and
/// its outcome commits while `b`'s body is held; the host is killed there.
/// A restarted boot of the same node (live) or another node (resume) claims
/// the turn from committed state alone: `a`'s body never runs again, `b`
/// reruns at its same call and attempt, and the turn settles once with the
/// combined output.
async fn s02(case: &mut Case) -> Result<()> {
    let fixture = case.fixture(
        json!([
            {"name": "a", "value": "A"},
            {"name": "b", "value": "B", "hold": true,
             "policy": {"type": "repeatable", "retry": {"max_attempts": 2, "backoff": {"base_delay_ms": 0, "max_delay_ms": 0}}}},
        ]),
        json!([{"calls": ["a", "b"]}, {"text": "A|B"}]),
    )?;
    let options = NodeOptions {
        fixture: Some(fixture),
        ..NodeOptions::default()
    };
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let turn = support::submit(case, "node-a", "turn-1", "run a and b").await?;
    case.control.wait_held("b", 1, case.deadline).await?;
    case.until("a's outcome committed", || async {
        Ok((support::applied(case, "node-a", "round.outcome")? >= 1).then_some(()))
    })
    .await?;
    case.record_barrier(json!({"barrier": "a durable, b held", "node": "node-a"}));
    case.kill("node-a", "a's outcome committed, b's body held")
        .await?;
    let cut = support::record_facts(case, &turn.run, "after the kill").await?;
    let outcomes = cut["records"]
        .as_array()
        .map(|records| {
            records
                .iter()
                .filter(|record| record["kind"] == "x_outcome")
                .count()
        })
        .unwrap_or_default();
    ensure!(
        outcomes == 1,
        "the cut holds {outcomes} committed outcomes, not a's one: {cut}"
    );
    ensure!(
        cut["end"].is_null(),
        "the turn ended before the kill: {cut}"
    );
    case.control.release("b");
    let next = successor(case);
    case.boot(Host::Consumer, next, options).await?;
    let outcome = support::follow(case, next, &turn.input).await?;
    if case.live_replay {
        // The killed boot's live activity is in the shared store, so the
        // restarted node follows it without a gap.
        ensure!(
            outcome["gaps"].as_array().is_none_or(Vec::is_empty),
            "a node on the shared live replay store reports gaps: {outcome}"
        );
    }
    let (kind, reply) = support::settled(&outcome);
    ensure!(kind == "completed", "the turn settled {kind}: {outcome}");
    ensure!(reply.as_deref() == Some("A|B"), "the reply is {reply:?}");
    let a = support::entries(case, "a")?;
    let b = support::entries(case, "b")?;
    ensure!(a.len() == 1, "a's body ran {} times: {a:?}", a.len());
    ensure!(
        b.len() == 2 && b[0] == b[1],
        "b ran {b:?}, not twice under one call and attempt"
    );
    support::presented(case, &json!({"a-0": "A", "b-0": "B"}))?;
    case.stop(next).await?;
    let end = support::record_facts(case, &turn.run, "after the turn settled").await?;
    ensure!(
        end["end"]["kind"] == "Answered",
        "the store's turn end is {end}"
    );
    // The store's own rows: one outcome per call, a's written by the killed
    // boot's epoch and b's by the claimer's.
    let outcomes = support::outcome_epochs(&end);
    ensure!(
        outcomes.len() == 2 && outcomes[&a[0].0] < outcomes[&b[0].0],
        "the store holds outcomes {outcomes:?}"
    );
    Ok(())
}

case!(
    s03_ambiguous_side_effect_dedups_externally,
    SqliteFile,
    Live,
    s03
);
case!(
    s03_ambiguous_side_effect_dedups_externally_resume,
    SqliteFile,
    Resume,
    s03
);

/// A `Repeatable` body writes a keyed mutation to the case's effect
/// recipient and is held after the recipient accepted it, before its
/// outcome commits; the node dies there. The node that claims the turn runs
/// the body again under the same call and attempt, so the recipient sees two
/// deliveries of one key and applies one mutation, and the turn settles
/// once.
async fn s03(case: &mut Case) -> Result<()> {
    let fixture = case.fixture(
        json!([{"name": "write", "value": "W", "effect": true, "hold": true,
                "policy": support::repeatable(2, 0)}]),
        json!([{"calls": ["write"]}, {"text": "written"}]),
    )?;
    let options = NodeOptions {
        fixture: Some(fixture),
        ..NodeOptions::default()
    };
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let turn = support::submit(case, "node-a", "turn-1", "write once").await?;
    case.control.wait_held("write", 1, case.deadline).await?;
    ensure!(
        case.control.effects().len() == 1,
        "the recipient did not accept the write first"
    );
    case.kill("node-a", "the recipient accepted, the outcome uncommitted")
        .await?;
    let cut = support::record_facts(case, &turn.run, "after the kill").await?;
    ensure!(
        support::records(&cut, "x_start").len() == 1
            && support::records(&cut, "x_outcome").is_empty(),
        "the cut is not a started body without an outcome: {cut}"
    );
    case.control.release("write");
    let next = successor(case);
    case.boot(Host::Consumer, next, options).await?;
    let outcome = support::follow(case, next, &turn.input).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("written"),
        "the turn settled {kind} {reply:?}"
    );
    let entries = support::entries(case, "write")?;
    ensure!(
        entries.len() == 2 && entries[0] == entries[1],
        "the body ran {entries:?}, not twice under one identity"
    );
    let effects = case.control.effects();
    let keys: std::collections::BTreeSet<_> = effects
        .iter()
        .map(|effect| (effect.call_id.clone(), effect.attempt))
        .collect();
    ensure!(
        effects.len() == 2 && keys.len() == 1,
        "the recipient saw {effects:?}, not one key twice"
    );
    case.evidence.effects.push(
        json!({"recipient": "keyed write", "deliveries": effects.len(), "mutations": keys.len()}),
    );
    case.stop(next).await?;
    let end = support::record_facts(case, &turn.run, "after the turn settled").await?;
    ensure!(
        end["end"]["kind"] == "Answered" && support::outcome_epochs(&end).len() == 1,
        "the store's turn end is {end}"
    );
    Ok(())
}

case!(
    s05_opposite_order_parallel_results_replay,
    SqliteFile,
    Live,
    s05
);
case!(
    s05_opposite_order_parallel_results_replay_resume,
    SqliteFile,
    Resume,
    s05
);
case!(
    s05_opposite_order_parallel_results_replay_postgresql,
    Postgresql,
    Live,
    s05
);
case!(
    s05_opposite_order_parallel_results_replay_postgresql_resume,
    Postgresql,
    Resume,
    s05
);

/// One standard `batch` of `a`, `b` and `c`: every body starts before any
/// finishes, they finish `c`, then `a`, and the node dies with `b` still
/// running. The claimer keeps the recorded batch plan, reruns only `b`, and
/// presents the results in source order.
async fn s05(case: &mut Case) -> Result<()> {
    let tool = |name: &str, value: &str| json!({"name": name, "value": value, "hold": true, "policy": support::repeatable(2, 0)});
    let fixture = case.fixture(
        json!([tool("a", "A"), tool("b", "B"), tool("c", "C")]),
        json!([{"batch": ["a", "b", "c"]}, {"text": "A|B|C"}]),
    )?;
    let options = NodeOptions {
        fixture: Some(fixture),
        ..NodeOptions::default()
    };
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let turn = support::submit(case, "node-a", "turn-1", "batch a b c").await?;
    for tool in ["a", "b", "c"] {
        case.control.wait_held(tool, 1, case.deadline).await?;
    }
    case.record_barrier(json!({"barrier": "all three bodies started"}));
    case.control.release("c");
    case.until("c's outcome committed", || async {
        Ok((support::applied(case, "node-a", "round.outcome")? >= 1).then_some(()))
    })
    .await?;
    case.control.release("a");
    case.until("a's outcome committed", || async {
        Ok((support::applied(case, "node-a", "round.outcome")? >= 2).then_some(()))
    })
    .await?;
    case.kill("node-a", "c and a durable, b running").await?;
    let cut = support::record_facts(case, &turn.run, "after the kill").await?;
    ensure!(
        support::records(&cut, "x_start").len() == 3 && support::outcome_epochs(&cut).len() == 2,
        "the cut is not three started bodies with two outcomes: {cut}"
    );
    case.control.release("b");
    let next = successor(case);
    case.boot(Host::Consumer, next, options).await?;
    let outcome = support::follow(case, next, &turn.input).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("A|B|C"),
        "the turn settled {kind} {reply:?}"
    );
    for (tool, runs) in [("a", 1), ("b", 2), ("c", 1)] {
        let entries = support::entries(case, tool)?;
        ensure!(
            entries.len() == runs && entries.windows(2).all(|pair| pair[0] == pair[1]),
            "{tool} ran {entries:?}"
        );
    }
    let calls = case.model_calls()?;
    let presented = calls
        .last()
        .map(|call| call["results"]["batch-0"].clone())
        .unwrap_or_default();
    let presented: serde_json::Value = serde_json::from_str(presented.as_str().unwrap_or("null"))?;
    let order: Vec<_> = presented["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            (
                row["index"].clone(),
                row["tool"].clone(),
                row["result"].clone(),
            )
        })
        .collect();
    ensure!(
        order
            == vec![
                (json!(0), json!("a"), json!("A")),
                (json!(1), json!("b"), json!("B")),
                (json!(2), json!("c"), json!("C"))
            ],
        "the batch presented {presented}"
    );
    case.stop(next).await?;
    let end = support::record_facts(case, &turn.run, "after the turn settled").await?;
    ensure!(
        end["end"]["kind"] == "Answered" && support::outcome_epochs(&end).len() == 3,
        "the store's turn end is {end}"
    );
    Ok(())
}

case!(s30_external_consumer_contract, SqliteMemory, Live, s30);
case!(
    s30_external_consumer_contract_resume,
    SqliteFile,
    Resume,
    s30
);

/// A consumer that depends on `lash` alone accepts a turn whose tool body
/// is held, loses its first follower, reattaches by input id, and races a
/// cancel against the terminal: one public terminal, the one the cancel
/// decision names. The resume leg kills the node while the body is held and
/// settles the turn on another node.
async fn s30(case: &mut Case) -> Result<()> {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(&std::env::var("LASH_E2E_REPO")?)
            .join("examples/e2e-consumer/Cargo.toml"),
    )?;
    let private: Vec<_> = manifest
        .lines()
        .filter(|line| {
            line.trim_start().starts_with("lash-") || line.contains("package = \"lash-internal")
        })
        .collect();
    ensure!(
        private.is_empty(),
        "the consumer imports private crates: {private:?}"
    );
    case.boot(Host::Consumer, "node-a", NodeOptions::default())
        .await?;
    let session = support::session(case);
    let host = case.node("node-a")?;
    let accepted = host
        .post(
            &format!("/sessions/{session}/inputs"),
            &json!({"id": "turn-1", "text": "hold:one"}),
        )
        .await?;
    let input = accepted["input_id"].as_str().unwrap_or_default().to_owned();
    case.record_barrier(json!({"barrier": "accepted", "receipt": accepted}));
    let held = case
        .until("the echo body entered", || async {
            let entered = case.node("node-a")?.get("/control/entered").await?;
            Ok(entered
                .as_array()
                .filter(|entered| !entered.is_empty())
                .cloned())
        })
        .await?;
    case.record_barrier(json!({"barrier": "echo entered", "entered": held}));
    // The first follower goes away before the terminal.
    let first = case.node("node-a")?.url.clone();
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        reqwest::Client::builder()
            .no_proxy()
            .build()?
            .get(format!("{first}/sessions/{session}/inputs/{input}"))
            .send(),
    )
    .await;
    ensure!(
        dropped.is_err(),
        "the first follower settled while the body was held"
    );
    let node = if case.leg == Leg::Resume {
        case.kill("node-a", "echo body held").await?;
        case.boot(Host::Consumer, "node-b", NodeOptions::default())
            .await?;
        "node-b"
    } else {
        "node-a"
    };
    let entered = case.node(node)?.get("/control/entered").await?;
    // The release and the cancel race; whichever the cancel decision names
    // is the one terminal.
    let host = case.node(node)?;
    let release = json!("hold:one");
    let empty = json!({});
    let cancel_path = format!("/sessions/{session}/inputs/{input}/cancel");
    let (released, receipt) = tokio::join!(
        host.post("/control/release", &release),
        host.post(&cancel_path, &empty),
    );
    released?;
    let receipt = receipt?;
    case.evidence
        .faults
        .push(json!({"fault": "cancel race", "receipt": receipt, "entered": entered}));
    let outcome = support::follow(case, node, &input).await?;
    let again = support::follow(case, node, &input).await?;
    // The live activities a follower may also see are not the terminal; the
    // settled report is.
    let terminal = |outcome: &serde_json::Value| {
        let mut report = outcome["report"].clone();
        if let Some(report) = report.as_object_mut() {
            report.remove("activities");
        }
        report
    };
    ensure!(
        terminal(&again) == terminal(&outcome),
        "a reattached follower saw another terminal: {again}"
    );
    let (kind, _) = support::settled(&outcome);
    // A requested cancel is the decision the terminal obeys; one that found
    // the turn settled leaves it completed.
    let requested = receipt.to_string().contains("Requested");
    ensure!(
        (kind == "cancelled") == requested,
        "the terminal {kind} contradicts the cancel decision {receipt}"
    );
    ensure!(
        matches!(kind.as_str(), "cancelled" | "completed"),
        "the turn settled {kind}: {outcome}"
    );
    let status = case.node(node)?.get("/control/drain-status").await?;
    ensure!(
        status["in_flight_turns"] == 0 && status["remaining_invocations"] == 0,
        "work remains after the terminal: {status}"
    );
    case.stop(node).await?;
    Ok(())
}
