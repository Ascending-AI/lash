//! S24/S25: L12/L19 across separately materialized candidate/synthetic-next hosts.
//! S33 deliberately selects the existing Phase A operator/wire/drain legs.

use anyhow::{Context, Result, ensure};
use lash_upgrade_harness::harness::{Case, NodeBinary, NodeBuilds, Services, block_on, wait_for};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::plugin_upgrade::{Entry, OTHER, PLUGIN};
use serde_json::{Value, json};

fn services() -> Result<Services> {
    Ok(Services {
        ingress_url: std::env::var("RESTATE_INGRESS_URL")
            .context("private live Restate ingress")?,
        admin_url: std::env::var("RESTATE_ADMIN_URL").context("private live Restate admin")?,
        postgres_url: String::new(),
    })
}

fn entries(case: &Case) -> Result<Vec<Entry>> {
    let path = case.gate_dir().join("entries.jsonl");
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn control(
    node: &NodeBinary,
    case: &Case,
    action: &str,
    session: &str,
    extra: &[&str],
) -> Result<Value> {
    let mut args = vec!["--action", action, "--session", session];
    args.extend(extra);
    node.plugin_upgrade(case, &args)
}

fn quiesce(case: &Case) -> Result<()> {
    let view = case.view()?;
    wait_for("the plugin Run and its close to settle", || {
        Ok(block_on(view.open_invocations())?.is_empty().then_some(()))
    })
}

fn counts(case: &Case, build: BuildLabel, phase: &str) -> Result<usize> {
    Ok(entries(case)?
        .iter()
        .filter(|entry| entry.build == build && entry.phase == phase)
        .count())
}

fn state_value(state: &Value, plugin: &str, key: &str) -> Option<String> {
    state["state"][plugin]["values"][key]
        .as_str()
        .map(str::to_owned)
}

fn frontier(state: &Value, plugin: &str) -> Result<lash_core::tool_run::StateFrontier> {
    serde_json::from_value(state["state"][plugin]["publication"].clone()).map_err(Into::into)
}

fn record(case: &Case, name: &str, value: &Value) -> Result<()> {
    std::fs::write(
        case.gate_dir().join(name),
        serde_json::to_vec_pretty(value)?,
    )?;
    Ok(())
}

fn same_terminal(first: &Value, replayed: &Value) -> Result<()> {
    let first: lash::SendOutcome = serde_json::from_value(first.clone())?;
    let replayed: lash::SendOutcome = serde_json::from_value(replayed.clone())?;
    ensure!(first.status() == replayed.status() && first.run() == replayed.run());
    // Live activity gaps belong to each follower. The durable result is the
    // authoritative terminal, including its acceptance and checkpoint refs.
    ensure!(
        serde_json::to_value(&first.output().context("settled output")?.result)?
            == serde_json::to_value(&replayed.output().context("replayed output")?.result)?,
        "reattachment changed the durable terminal"
    );
    Ok(())
}

fn one_turn(node: &NodeBinary, case: &Case, session: &str) -> Result<Value> {
    let accepted = control(node, case, "send", session, &["--variant", "single"])?;
    let input = accepted["input_id"]
        .as_str()
        .context("durable input identity")?;
    let terminal = control(node, case, "follow", session, &["--input", input])?;
    record(case, &format!("terminal-{}.json", node.label()), &terminal)?;
    ensure!(
        serde_json::from_value::<lash::SendOutcome>(terminal.clone())?.status()
            == lash::TurnStatus::Answered,
        "plugin turn did not answer: {terminal}"
    );
    quiesce(case)?;
    let first = control(node, case, "read", session, &[])?;
    let before = entries(case)?;
    // Two cold processes read the checkpoint and reattach the completed input.
    // Neither read is a fresh submission, so completed code must not run again.
    for _ in 0..2 {
        let reopened = control(node, case, "read", session, &[])?;
        ensure!(
            reopened == first,
            "cold reopen changed the checkpoint/frontier"
        );
        let replayed = control(node, case, "follow", session, &["--input", input])?;
        same_terminal(&terminal, &replayed)?;
    }
    let after = entries(case)?;
    ensure!(
        serde_json::to_value(after)? == serde_json::to_value(before)?,
        "cold follow reentered completed code"
    );
    let publication = frontier(&first, PLUGIN)?;
    ensure!(
        publication.applied.is_some() && !publication.receipts.is_empty(),
        "no durable state frontier"
    );
    Ok(first)
}

#[test]
#[ignore = "needs exact candidate/synthetic-next binaries and private live Restate"]
fn s24_plugin_revision_rolls_back_without_reentering_completed_work() -> Result<()> {
    let builds = NodeBuilds::from_env()?;
    let scratch = std::path::PathBuf::from(
        std::env::var_os("LASH_PHASE_A_ARTIFACT_DIR")
            .context("persistent scenario artifact directory")?,
    )
    .join(format!("s24-{}", std::process::id()));
    let case = Case::sqlite("s24", &services()?, &scratch)?;
    let session = case.session_id("plugin");
    let n = builds.n.serve_plugin_upgrade(&case, None)?;
    let n_generation = n.generation()?.to_owned();
    let first = one_turn(&builds.n, &case, &session)?;
    ensure!(
        state_value(&first, PLUGIN, "value").as_deref() == Some("N")
            && state_value(&first, PLUGIN, "hooks").as_deref() == Some("H"),
        "candidate state: {first}"
    );
    ensure!(
        counts(&case, BuildLabel::N, "body")? == 1
            && counts(&case, BuildLabel::N, "reducer")? == 2
            && counts(&case, BuildLabel::N, "hook")? == 1
    );

    // A's body has entered under N's recorded binding before redeployment.
    let pending = control(&builds.n, &case, "send", &session, &["--variant", "same"])?;
    wait_for(
        "the admitted predecessor tool body",
        || match std::fs::read(case.gate_dir().join("A.entered")) {
            Ok(bytes) => Ok(Some(serde_json::from_slice::<Value>(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        },
    )?;
    wait_for("B to complete while A remains pending", || {
        Ok(entries(&case)?
            .iter()
            .any(|entry| entry.phase == "reducer" && entry.detail == "B")
            .then_some(()))
    })?;
    // Keep the predecessor deployment while the successor becomes newest.
    let next = builds.next.serve_plugin_upgrade(&case, None)?;
    std::fs::write(
        case.gate_dir().join("A.release"),
        b"retained predecessor callback",
    )?;
    let input = pending["input_id"].as_str().context("pending input")?;
    let pending_terminal = control(&builds.next, &case, "follow", &session, &["--input", input])?;
    ensure!(
        serde_json::from_value::<lash::SendOutcome>(pending_terminal.clone())?.status()
            == lash::TurnStatus::Answered,
        "old callback did not drain on N: {pending_terminal}"
    );
    quiesce(&case)?;
    let drained = control(&builds.next, &case, "read", &session, &[])?;
    ensure!(
        state_value(&drained, PLUGIN, "value").as_deref() == Some("NBA")
            && state_value(&drained, PLUGIN, "hooks").as_deref() == Some("HH"),
        "pending callbacks substituted the successor reducer: {drained}"
    );
    ensure!(
        counts(&case, BuildLabel::Next, "body")? == 0
            && counts(&case, BuildLabel::Next, "reducer")? == 0
            && counts(&case, BuildLabel::Next, "hook")? == 0,
        "new revision executed predecessor work"
    );
    // Next prepends S; rollback appends N. These operations do not commute.
    ensure!(
        next.generation()? != n_generation,
        "revision changed without changing the executable lane"
    );
    let rolled = one_turn(&builds.next, &case, &session)?;
    ensure!(
        state_value(&rolled, PLUGIN, "value").as_deref() == Some("SNBA")
            && state_value(&rolled, PLUGIN, "hooks").as_deref() == Some("JHH"),
        "successor reducer/order: {rolled}"
    );
    ensure!(
        first["config"] == rolled["config"],
        "redeploy changed recorded configuration"
    );
    ensure!(
        counts(&case, BuildLabel::Next, "body")? == 1
            && counts(&case, BuildLabel::Next, "reducer")? == 2
            && counts(&case, BuildLabel::Next, "hook")? == 1
    );
    ensure!(
        entries(&case)?
            .iter()
            .filter(|entry| entry.build == BuildLabel::Next && entry.plugin == PLUGIN)
            .all(|entry| entry.converter_calls == 2),
        "successor must convert state and config once before completed work"
    );
    ensure!(
        entries(&case)?
            .iter()
            .filter(|entry| entry.build == BuildLabel::N)
            .all(|entry| entry.converter_calls == 0),
        "candidate unexpectedly converted its native format"
    );
    ensure!(
        counts(&case, BuildLabel::N, "body")? == 3 && counts(&case, BuildLabel::N, "hook")? == 2,
        "redeploy reentered predecessor callbacks"
    );

    let rollback = builds.n.serve_plugin_upgrade(&case, None)?;
    ensure!(
        rollback.generation()? == n_generation
            && rollback.uri()? != n.uri()?
            && rollback.uri()? != next.uri()?,
        "rollback lost the predecessor lane or reused a URI"
    );
    let onward = one_turn(&builds.n, &case, &session)?;
    ensure!(
        state_value(&onward, PLUGIN, "value").as_deref() == Some("SNBAN")
            && state_value(&onward, PLUGIN, "hooks").as_deref() == Some("JHHH"),
        "rollback reducer/order: {onward}"
    );
    ensure!(
        counts(&case, BuildLabel::N, "body")? == 4
            && counts(&case, BuildLabel::N, "reducer")? == 7
            && counts(&case, BuildLabel::N, "hook")? == 3
    );
    record(
        &case,
        "s24-frontiers.json",
        &json!({"candidate":first,"successor":rolled,"rollback":onward}),
    )?;
    rollback.stop()?;
    next.stop()?;
    n.stop()?;
    Ok(())
}

/// S25 shares the controller's real D-durable barrier. The adapter supplies
/// the decoded B decision; a reducer/body ledger is never its substitute.
/// This entrypoint is registered once the concrete controller can bind that cut.
pub fn s25_cold_reopen(
    case: &Case,
    builds: &NodeBuilds,
    variant: &str,
    await_b_durable: impl FnOnce(
        &Case,
        &Value,
    ) -> Result<lash_upgrade_harness::e2e::control::BarrierProof>,
) -> Result<()> {
    let session = case.session_id(variant);
    let node = builds.n.serve_plugin_upgrade(case, None)?;
    let bind = node.bind()?;
    let accepted = control(&builds.n, case, "send", &session, &["--variant", variant])?;
    let input = accepted["input_id"]
        .as_str()
        .context("durable input identity")?;
    wait_for("A's actual body entry", || {
        let bytes = std::fs::read(case.gate_dir().join("A.entered"));
        match bytes {
            Ok(bytes) => Ok(Some(serde_json::from_slice::<Value>(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    })?;
    let proof = await_b_durable(case, &accepted)?;
    ensure!(
        proof.barrier.kind == lash_upgrade_harness::e2e::control::BarrierKind::DDurable
            && proof.journal_index.is_some(),
        "S25 requires a decoded durable B decision"
    );
    record(case, "s25-cut.json", &serde_json::to_value(&proof)?)?;
    node.stop()?;

    // The successor controls cancellation. The predecessor keeps its drain
    // lane to finish its own journal, rather than decoding it as a new Run.
    let cancellation_host = builds.next.serve_plugin_upgrade(case, None)?;
    control(&builds.next, case, "cancel", &session, &["--input", input])?;
    let predecessor = builds.n.serve_plugin_upgrade(case, Some(&bind))?;
    std::fs::write(case.gate_dir().join("A.release"), b"release after cancel")?;
    let terminal = control(&builds.next, case, "follow", &session, &["--input", input])?;
    ensure!(
        serde_json::from_value::<lash::SendOutcome>(terminal.clone())?.status()
            == lash::TurnStatus::Cancelled,
        "cancelled A did not terminate: {terminal}"
    );
    quiesce(case)?;
    predecessor.stop()?;
    cancellation_host.stop()?;
    let successor = builds.next.serve_plugin_upgrade(case, None)?;
    let state = control(&builds.next, case, "read", &session, &[])?;
    let b_plugin = if variant == "namespace" {
        OTHER
    } else {
        PLUGIN
    };
    let b_key = if variant == "disjoint" { "b" } else { "value" };
    ensure!(
        state_value(&state, b_plugin, b_key).as_deref() == Some("B"),
        "durable B lost/duplicated or A leaked: {state}"
    );
    if variant == "disjoint" {
        ensure!(
            state["state"][PLUGIN]["values"].get("a").is_none(),
            "unrecorded A published"
        );
    }
    if variant == "namespace" {
        ensure!(
            state["state"][PLUGIN]["values"].get("value").is_none(),
            "A's namespace published"
        );
    }
    let frontier = frontier(&state, b_plugin)?;
    ensure!(
        frontier.applied.is_some() && frontier.receipts.len() == 1,
        "B's exact frontier was not retained: {frontier:?}"
    );
    let before = entries(case)?;
    for _ in 0..2 {
        ensure!(
            control(&builds.next, case, "read", &session, &[])? == state,
            "cold checkpoint read changed state/frontier"
        );
        same_terminal(
            &terminal,
            &control(&builds.next, case, "follow", &session, &["--input", input])?,
        )?;
    }
    let after = entries(case)?;
    ensure!(
        serde_json::to_value(after)? == serde_json::to_value(&before)?,
        "completed replay invoked code"
    );
    ensure!(
        before
            .iter()
            .filter(|entry| entry.phase == "body" && entry.detail["symbol"] == "B")
            .count()
            == 1,
        "durable B's body reran"
    );
    ensure!(
        before
            .iter()
            .filter(|entry| entry.phase == "reducer" && entry.detail == "B")
            .count()
            == 1
            && before
                .iter()
                .all(|entry| entry.phase != "reducer" || entry.detail != "A"),
        "durable B reduced again or cancelled A reduced"
    );
    record(
        case,
        "s25-final.json",
        &json!({"variant":variant,"state":state,"terminal":terminal,"entries":before}),
    )?;
    successor.stop()?;
    Ok(())
}
