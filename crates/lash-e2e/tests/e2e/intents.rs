//! Declared intents on a durable node: S08 (a cancel before the final
//! outcome prevents its declarations) and S09 (a cancel after it cannot
//! abandon them). A body's `EmitProcessEvent` intent is a store-local
//! effect: it commits in the transaction that records the call's outcome
//! (ADR 0132 §5), into the case's receiver process.

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, Leg, NodeOptions};
use serde_json::{Value, json};

use crate::support::{self, successor};

/// Start the case's receiver through node `node`, answering its id.
async fn receiver(case: &Case, node: &str) -> Result<String> {
    let session = support::session(case);
    // The session exists once a turn was sent; the receiver observes it.
    let started = case
        .node(node)?
        .post(&format!("/receiver/{session}"), &json!({}))
        .await?;
    started["process"]
        .as_str()
        .map(ToOwned::to_owned)
        .context("the receiver answered no process")
}

/// The case's declared events the receiver holds, read through node `node`.
async fn events(case: &mut Case, node: &str, process: &str) -> Result<Vec<Value>> {
    let events = case
        .node(node)?
        .get(&format!("/receiver/{process}/events"))
        .await?;
    case.evidence
        .effects
        .push(json!({"receiver": process, "events": events}));
    Ok(events["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["event_type"] == "e2e_mutation")
        .cloned()
        .collect())
}

/// Settle a `@warm` turn on node-a, so the session exists for the receiver.
async fn warm(case: &mut Case) -> Result<()> {
    let turn = support::submit(case, "node-a", "turn-0", "@warm open the session").await?;
    let outcome = support::follow(case, "node-a", &turn.input).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(
        kind == "completed",
        "the warm turn settled {kind}: {outcome}"
    );
    Ok(())
}

fn emitting(case: &Case, hold: bool) -> Result<NodeOptions> {
    Ok(NodeOptions {
        fixture: Some(case.scripted(
            json!([{"name": "declare", "value": "D", "hold": hold, "emit": true}]),
            json!([{"calls": ["declare"]}, {"text": "declared"}]),
            json!({"warm": [{"text": "warm"}]}),
        )?),
        ..NodeOptions::default()
    })
}

case!(
    s08_pre_final_cancel_prevents_declarations,
    SqliteFile,
    Live,
    s08
);
case!(
    s08_pre_final_cancel_prevents_declarations_resume,
    SqliteFile,
    Resume,
    s08
);
case!(
    s08_pre_final_cancel_prevents_declarations_postgresql,
    Postgresql,
    Live,
    s08
);
case!(
    s08_pre_final_cancel_prevents_declarations_postgresql_resume,
    Postgresql,
    Resume,
    s08
);

/// The body that declares an event is held before its result; the turn is
/// cancelled and the cancel recorded; then the body returns (live) or its
/// node dies and another settles the turn (resume). One cancel decision,
/// one cancelled terminal, and the receiver holds no event.
async fn s08(case: &mut Case) -> Result<()> {
    let options = emitting(case, true)?;
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    warm(case).await?;
    let process = receiver(case, "node-a").await?;
    let turn = support::submit(case, "node-a", "turn-1", "declare").await?;
    case.control.wait_held("declare", 1, case.deadline).await?;
    let receipt = support::cancel(case, "node-a", &turn.input).await?;
    ensure!(
        receipt.to_string().contains("Requested"),
        "the cancel was not recorded: {receipt}"
    );
    let node = match case.leg {
        Leg::Live => {
            case.control.release("declare");
            "node-a"
        }
        Leg::Resume => {
            case.kill("node-a", "cancel recorded, body held").await?;
            case.control.release("declare");
            let next = successor(case);
            case.boot(Host::Consumer, next, options).await?;
            next
        }
    };
    let outcome = support::follow(case, node, &turn.input).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(kind == "cancelled", "the turn settled {kind}: {outcome}");
    ensure!(
        support::entries(case, "declare")?.len() == 1,
        "the body ran again after the cancel"
    );
    let events = events(case, node, &process).await?;
    ensure!(events.is_empty(), "a cancelled call declared {events:?}");
    case.stop(node).await?;
    let end = support::record_facts(case, &turn.run, "after the cancel").await?;
    ensure!(
        end["end"]["kind"] == "Cancelled",
        "the store's turn end is {end}"
    );
    Ok(())
}

case!(
    s09_post_final_cancel_drains_protected_work_after_intent,
    SqliteFile,
    Live,
    s09
);
case!(
    s09_post_final_cancel_drains_protected_work_after_intent_resume,
    SqliteFile,
    Resume,
    s09
);
case!(
    s09_post_final_cancel_drains_protected_work_after_intent_postgresql,
    Postgresql,
    Live,
    s09
);
case!(
    s09_post_final_cancel_drains_protected_work_after_intent_postgresql_resume,
    Postgresql,
    Resume,
    s09
);

/// The call's final outcome and its declared event commit together; the
/// turn is held before it presents the result, and cancelled there. Live,
/// the hold is released; resume, the node dies and another settles the
/// turn. The committed final stays recorded, the receiver holds its one
/// event, and the cancel abandons nothing that committed.
async fn s09(case: &mut Case) -> Result<()> {
    let options = emitting(case, false)?;
    let held = NodeOptions {
        cuts: vec![json!({"label": "round.present+model.start", "before": true})],
        ..options.clone()
    };
    case.boot(Host::Consumer, "node-a", held).await?;
    warm(case).await?;
    let process = receiver(case, "node-a").await?;
    let turn = support::submit(case, "node-a", "turn-1", "declare").await?;
    case.held("node-a", "round.present+model.start").await?;
    ensure!(
        support::applied(case, "node-a", "round.outcome")? >= 1,
        "the final did not commit before the presentation"
    );
    let receipt = support::cancel(case, "node-a", &turn.input).await?;
    ensure!(
        receipt.to_string().contains("Requested"),
        "the cancel was not recorded: {receipt}"
    );
    let node = match case.leg {
        Leg::Live => {
            case.node("node-a")?
                .post("/control/cuts/release", &json!("round.present+model.start"))
                .await?;
            "node-a"
        }
        Leg::Resume => {
            case.kill(
                "node-a",
                "final and event durable, presentation held, cancel recorded",
            )
            .await?;
            let cut = support::record_facts(case, &turn.run, "after the kill").await?;
            ensure!(
                support::outcome_epochs(&cut).len() == 1,
                "the final is not durable at the cut: {cut}"
            );
            let next = successor(case);
            case.boot(Host::Consumer, next, options).await?;
            next
        }
    };
    let outcome = support::follow(case, node, &turn.input).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(
        matches!(kind.as_str(), "cancelled" | "completed"),
        "the turn settled {kind}: {outcome}"
    );
    ensure!(
        support::entries(case, "declare")?.len() == 1,
        "the committed body ran again"
    );
    let events = events(case, node, &process).await?;
    ensure!(
        events.len() == 1 && events[0]["payload"]["value"] == "D",
        "the receiver holds {events:?}, not the one committed event"
    );
    case.stop(node).await?;
    let end = support::record_facts(case, &turn.run, "after the turn settled").await?;
    ensure!(
        support::outcome_epochs(&end).len() == 1,
        "the committed final is not retained: {end}"
    );
    Ok(())
}
