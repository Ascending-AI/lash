//! Plugin state on a durable node: S04 (a reduced resolution is not a
//! publication until its outcome commits) and S25 (concurrent state
//! survives a killed node and cold reopens).

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, NodeOptions};
use serde_json::{Value, json};

use crate::support::{self, successor};

/// Run a turn of the case's `@read` script through `node`: it reads both
/// case plugins' published namespaces, and answers them.
async fn read_namespaces(case: &mut Case, node: &str, id: &str) -> Result<(Value, Value)> {
    let turn = support::submit(case, node, id, "@read the namespaces").await?;
    let outcome = support::follow(case, node, &turn.input).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(
        kind == "completed",
        "the read turn settled {kind}: {outcome}"
    );
    let calls = case.model_calls()?;
    let results = &calls.last().context("no model call")?["results"];
    let decode = |call: &str| -> Result<Value> {
        Ok(serde_json::from_str(results[call].as_str().with_context(
            || format!("no {call} result in {results}"),
        )?)?)
    };
    let namespaces = (decode("read_one-0")?, decode("read_two-0")?);
    case.evidence
        .outputs
        .push(json!({"namespaces": [namespaces.0, namespaces.1], "turn": id}));
    Ok(namespaces)
}

/// The `@read` script and its two reading tools.
fn readers() -> (Value, Value) {
    (
        json!([
            {"name": "read_one", "value": null, "reads": true},
            {"name": "read_two", "value": null, "reads": true, "plugin": "two"},
        ]),
        json!({"read": [{"calls": ["read_one", "read_two"]}, {"text": "read"}]}),
    )
}

/// The reductions the case's reducers ran.
fn reductions(case: &Case) -> Result<Vec<Value>> {
    lash_e2e::read_jsonl(&case.dir.join("reducers.jsonl"))
}

case!(
    s04_proposal_is_not_durable_publication,
    SqliteFile,
    Live,
    s04
);
case!(
    s04_proposal_is_not_durable_publication_resume,
    SqliteFile,
    Resume,
    s04
);
case!(
    s04_proposal_is_not_durable_publication_postgresql,
    Postgresql,
    Live,
    s04
);
case!(
    s04_proposal_is_not_durable_publication_postgresql_resume,
    Postgresql,
    Resume,
    s04
);

/// A `Repeatable` body returns its result with a state command; its plugin
/// reduces it privately and the outcome's commit is held, then the node
/// dies. Nothing was published: the store holds no namespace row and no
/// outcome. The claimer reruns the body, reduces again against the
/// published namespace, and publishes the one recorded resolution once.
async fn s04(case: &mut Case) -> Result<()> {
    let (mut tools, scripts) = readers();
    tools.as_array_mut().context("tools")?.push(json!({
        "name": "write", "value": "W", "policy": support::repeatable(2, 0),
        "state": [{"append": {"key": "log", "input": "A"}}],
    }));
    let fixture = case.scripted(
        tools,
        json!([{"calls": ["write"]}, {"text": "written"}]),
        scripts,
    )?;
    let options = NodeOptions {
        fixture: Some(fixture),
        ..NodeOptions::default()
    };
    let held = NodeOptions {
        cuts: vec![json!({"label": "round.outcome", "before": true})],
        ..options.clone()
    };
    case.boot(Host::Consumer, "node-a", held).await?;
    let turn = support::submit(case, "node-a", "turn-1", "write the log").await?;
    case.held("node-a", "round.outcome").await?;
    ensure!(
        reductions(case)?.len() == 1,
        "the proposal was not reduced first"
    );
    if support::store_readable_live(case) {
        let namespaces = support::namespaces(case, &turn.run).await?;
        ensure!(
            !support::published(&namespaces, "e2e-case")?,
            "a held proposal is visible: {namespaces:?}"
        );
    }
    case.kill("node-a", "outcome commit held after the reduction")
        .await?;
    let cut = support::record_facts(case, &turn.run, "after the kill").await?;
    ensure!(
        support::outcome_epochs(&cut).is_empty(),
        "an outcome committed before the kill: {cut}"
    );
    let namespaces = support::namespaces(case, &turn.run).await?;
    ensure!(
        !support::published(&namespaces, "e2e-case")?,
        "the dead node's proposal was published: {namespaces:?}"
    );
    let next = successor(case);
    case.boot(Host::Consumer, next, options).await?;
    let outcome = support::follow(case, next, &turn.input).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("written"),
        "the turn settled {kind} {reply:?}"
    );
    let writes = support::entries(case, "write")?;
    ensure!(
        writes.len() == 2 && writes[0] == writes[1],
        "the body ran {writes:?}"
    );
    let (one, _) = read_namespaces(case, next, "turn-2").await?;
    ensure!(
        one["values"] == json!({"log": ["A"]}),
        "the namespace is {one}, not one publication"
    );
    case.stop(next).await?;
    Ok(())
}

/// One S25 variant: where `a`'s and `b`'s commands land.
#[derive(Clone, Copy)]
enum Keys {
    Same,
    Disjoint,
    Namespaces,
}

case!(
    s25_concurrent_state_survives_cold_reopen_same_key,
    SqliteFile,
    Live,
    s25_same
);
case!(
    s25_concurrent_state_survives_cold_reopen_disjoint_keys,
    SqliteFile,
    Live,
    s25_disjoint
);
case!(
    s25_concurrent_state_survives_cold_reopen_namespaces,
    SqliteFile,
    Live,
    s25_namespaces
);
case!(
    s25_concurrent_state_survives_cold_reopen_same_key_resume,
    SqliteFile,
    Resume,
    s25_same
);
case!(
    s25_concurrent_state_survives_cold_reopen_disjoint_keys_resume,
    SqliteFile,
    Resume,
    s25_disjoint
);
case!(
    s25_concurrent_state_survives_cold_reopen_namespaces_resume,
    SqliteFile,
    Resume,
    s25_namespaces
);
case!(
    s25_concurrent_state_survives_cold_reopen_same_key_postgresql,
    Postgresql,
    Live,
    s25_same
);
case!(
    s25_concurrent_state_survives_cold_reopen_disjoint_keys_postgresql,
    Postgresql,
    Live,
    s25_disjoint
);
case!(
    s25_concurrent_state_survives_cold_reopen_namespaces_postgresql,
    Postgresql,
    Live,
    s25_namespaces
);
case!(
    s25_concurrent_state_survives_cold_reopen_same_key_postgresql_resume,
    Postgresql,
    Resume,
    s25_same
);
case!(
    s25_concurrent_state_survives_cold_reopen_disjoint_keys_postgresql_resume,
    Postgresql,
    Resume,
    s25_disjoint
);
case!(
    s25_concurrent_state_survives_cold_reopen_namespaces_postgresql_resume,
    Postgresql,
    Resume,
    s25_namespaces
);

async fn s25_same(case: &mut Case) -> Result<()> {
    s25(case, Keys::Same).await
}

async fn s25_disjoint(case: &mut Case) -> Result<()> {
    s25(case, Keys::Disjoint).await
}

async fn s25_namespaces(case: &mut Case) -> Result<()> {
    s25(case, Keys::Namespaces).await
}

/// Two members of one round change state: `a`'s body is held before it
/// returns (no candidate is recorded), `b`'s result and its resolution are
/// durable. The node is SIGKILLed there; `a` is `Once`, so the claimer
/// records it interrupted and never runs it. `b`'s recorded resolution is
/// installed without running its reducer again; `a`'s commands are absent.
/// Two further cold reopens read the same namespaces.
async fn s25(case: &mut Case, keys: Keys) -> Result<()> {
    let (a_key, b_key, b_plugin) = match keys {
        Keys::Same => ("k", "k", "one"),
        Keys::Disjoint => ("ka", "kb", "one"),
        Keys::Namespaces => ("k", "k", "two"),
    };
    let (mut tools, scripts) = readers();
    let list = tools.as_array_mut().context("tools")?;
    list.push(json!({"name": "a", "value": "A", "hold": true,
                     "state": [{"append": {"key": a_key, "input": "A"}}]}));
    list.push(json!({"name": "b", "value": "B", "plugin": b_plugin,
                     "state": [{"append": {"key": b_key, "input": "B"}}]}));
    let fixture = case.scripted(
        tools,
        json!([{"calls": ["a", "b"]}, {"text": "settled"}]),
        scripts,
    )?;
    let options = NodeOptions {
        fixture: Some(fixture),
        ..NodeOptions::default()
    };
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let turn = support::submit(case, "node-a", "turn-1", "change state").await?;
    case.control.wait_held("a", 1, case.deadline).await?;
    case.until("b's outcome committed", || async {
        Ok((support::applied(case, "node-a", "round.outcome")? >= 1).then_some(()))
    })
    .await?;
    case.kill("node-a", "a unrecorded and held, b durable")
        .await?;
    support::record_facts(case, &turn.run, "after the kill").await?;
    case.control.release("a");
    let next = successor(case);
    case.boot(Host::Consumer, next, options.clone()).await?;
    let outcome = support::follow(case, next, &turn.input).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(kind == "completed", "the turn settled {kind}: {outcome}");
    ensure!(
        support::entries(case, "a")?.len() == 1,
        "a's body ran again"
    );
    ensure!(
        support::entries(case, "b")?.len() == 1,
        "b's body ran again"
    );
    let (b_owner, b_other) = (
        if b_plugin == "two" { 1 } else { 0 },
        if b_plugin == "two" { 0 } else { 1 },
    );
    let mut reads = Vec::new();
    for (index, id) in ["turn-2", "turn-3", "turn-4"].into_iter().enumerate() {
        if index > 0 {
            // A cold reopen: the node stops and a new boot reads the store.
            case.stop(next).await?;
            case.boot(Host::Consumer, next, options.clone()).await?;
        }
        let (one, two) = read_namespaces(case, next, id).await?;
        let namespaces = [one, two];
        ensure!(
            namespaces[b_owner]["values"] == json!({b_key: ["B"]}),
            "b's namespace is {}, not B exactly once and A absent",
            namespaces[b_owner]
        );
        ensure!(
            namespaces[b_other]["values"] == json!({}),
            "the other namespace is {}",
            namespaces[b_other]
        );
        reads.push(namespaces);
    }
    ensure!(
        reads.windows(2).all(|pair| pair[0] == pair[1]),
        "a reopen changed the state: {reads:?}"
    );
    let reductions = reductions(case)?;
    ensure!(
        reductions.len() == 1 && reductions[0]["input"] == "B",
        "the reducers ran {reductions:?}, not b's one reduction"
    );
    case.stop(next).await?;
    Ok(())
}
