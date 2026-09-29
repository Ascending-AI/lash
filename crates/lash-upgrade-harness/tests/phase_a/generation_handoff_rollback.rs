//! `generation_handoff_rollback` (ADR 0115 §6): journals and deployments
//! across a generation hand-off and a rollback, over real processes.
//!
//! - **A foreign-`G` journal parks.** A root's journal is started by N and
//!   held at the provider. N's process dies, and N+1 is started behind N's
//!   URI without registering: the code behind N's deployment is swapped, the
//!   operator mistake ADR 0043 forbids. Restate replays the root on N+1,
//!   whose sentinel parks it `RetiredGeneration`, naming N's `G`, and N+1
//!   makes no model call. Swapped back, N resumes the kept journal and the
//!   turn answers.
//! - **A pinned drive's root runs on N+1.** A drive pinned to N holds its
//!   first root while a second input arrives and N+1 registers. The drive
//!   admits the second root, whose `LashTurn` runs on N+1: answered, with
//!   no refusal and no `SubstrateLost`, and each root makes one model call.
//! - **Signals that race the hand-off are delivered once.** Four processes
//!   wait for a signal on N. One is signalled and ends before N's
//!   generation drains; the others are signalled as the drain hands their
//!   waits to N+1: at once, from both builds with one signal id, and after
//!   the successor waits again on N+1. Each ends with its signal, records
//!   it once, and runs at most one successor, on N+1.
//! - **Registration is guarded.** N+1 registering at N's URI is refused
//!   typed, and nothing is registered.
//! - **Rollback registers N at a fresh URI**, beside N+1's deployment, and
//!   N answers.
//! - **A continuation N cannot decode stays on N+1.** A process N+1 started
//!   waits across the rollback, parked in N+1's continuation format. N
//!   signals it, and every segment it runs stays on N+1's deployment.
//!
//! Every "zero effects" and "exactly once" counts the model calls in the
//! effects log, or the signal events a process recorded; every "runs on"
//! reads the invocation's pinned deployment.

use anyhow::{Context, Result, bail, ensure};
use lash_upgrade_harness::harness::{
    Case, LASHCTL_N_ENV, LASHCTL_NEXT_ENV, NodeBinary, Operator, ServeOptions, ServingNode,
    block_on, wait_for,
};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::process::{HarnessReply, SIGNAL};
use lash_upgrade_harness::node::{EndpointServesAnotherGeneration, served_by};
use lash_upgrade_harness::restate_view::{ProcessSegment, RestateView};
use serde_json::json;

use crate::support::{Leg, record};

/// The root journal N starts parks on N+1 with zero effects, and completes
/// on N once N serves its URI again. Answers the restored N node.
fn foreign_journal_parks(
    leg: &Leg,
    case: &Case,
    view: &RestateView,
    n_node: ServingNode,
) -> Result<ServingNode> {
    let (n, next) = (&leg.builds.n, &leg.builds.next);
    let g_n = n_node.generation()?.to_owned();
    let session = case.session_id("foreign");
    let gate = "foreign-journal";
    let pending = n.spawn_turn(case, &session, &format!("hold:{gate} a root N journals"))?;
    let held_by = case.await_gate(gate)?;
    ensure!(
        held_by == g_n,
        "the root was held by generation {held_by}, not N's {g_n}"
    );

    // N dies, and N+1 answers at N's URI unregistered.
    let bind = n_node.bind()?;
    n_node.stop()?;
    let imposter = next.serve_with(
        case,
        &ServeOptions {
            bind: Some(bind.clone()),
            unregistered: true,
            ..ServeOptions::default()
        },
    )?;
    let g_next = imposter.generation()?.to_owned();
    // Restate reports an attempt's failure while the invocation backs off,
    // and pauses it once the turn handler's attempts are spent.
    let seen = std::cell::RefCell::new(Vec::new());
    let (key, refused) = wait_for("N+1 to refuse the root's journal", || {
        let runs = block_on(view.invocations_like("LashTurn", &session, "run"))?;
        seen.replace(runs.clone());
        Ok(runs.into_iter().find(|(_, run)| {
            run.last_failure.as_deref().is_some_and(|failure| {
                failure.contains("RetiredGeneration") && failure.contains(&g_n)
            })
        }))
    })
    .with_context(|| format!("the root's runs: {:?}", seen.borrow()))?;
    record(
        leg,
        "foreign-journal-refused.json",
        &format!("{key}: {refused:?}"),
    )?;
    let parked = wait_for("the refused journal to pause", || {
        let runs = block_on(view.invocations("LashTurn", &key, "run"))?;
        Ok(runs
            .into_iter()
            .find(|run| run.id == refused.id && run.status == "paused"))
    })?;
    ensure!(
        refused
            .last_failure
            .as_deref()
            .is_some_and(|failure| failure.contains(&g_next)),
        "the park does not name the refusing generation {g_next}: {refused:?}"
    );
    let from_next: Vec<_> = case
        .effects_of(gate)?
        .into_iter()
        .filter(|effect| effect.build == BuildLabel::Next)
        .collect();
    ensure!(
        from_next.is_empty(),
        "N+1 made model calls for N's journal: {from_next:?}"
    );

    // Swapped back, N resumes the kept journal and the turn answers.
    case.release(gate)?;
    imposter.stop()?;
    let restored = n.serve_with(
        case,
        &ServeOptions {
            bind: Some(bind),
            unregistered: true,
            ..ServeOptions::default()
        },
    )?;
    block_on(view.resume(&parked.id))?;
    let turn = pending.wait()?;
    ensure!(
        turn.reply.as_deref() == Some(served_by(BuildLabel::N, &g_n).as_str()),
        "the resumed root answered {turn:?}"
    );
    let effects = case.effects_of(gate)?;
    ensure!(
        !effects.is_empty() && effects.iter().all(|effect| effect.build == BuildLabel::N),
        "the root's model calls were not all N's: {effects:?}"
    );
    Ok(restored)
}

/// A drive pinned to N admits a root that runs on N+1. Answers the N+1
/// node it registered.
fn pinned_drive_root(
    leg: &Leg,
    case: &Case,
    view: &RestateView,
    n_deployment: &str,
    g_n: &str,
) -> Result<ServingNode> {
    let (n, next) = (&leg.builds.n, &leg.builds.next);
    let session = case.session_id("pinned");
    let gate = "pinned-drive";
    let first_marker = format!("hold:{gate} the drive's first root");
    let second_marker = "the root the pinned drive admits next";
    let first = n.spawn_turn(case, &session, &first_marker)?;
    case.await_gate(gate)?;
    let second = n.spawn_turn(case, &session, second_marker)?;
    wait_for("the second input's drive request", || {
        Ok(
            (block_on(view.invocations("LashSession", &session, "drive"))?.len() >= 2)
                .then_some(()),
        )
    })?;
    let next_node = next.serve(case)?;
    let g_next = next_node.generation()?.to_owned();
    let next_deployment = block_on(view.deployment_at(next_node.uri()?))?;
    case.release(gate)?;

    let first = first.wait()?;
    ensure!(
        first.reply.as_deref() == Some(served_by(BuildLabel::N, g_n).as_str()),
        "the first root answered {first:?}"
    );
    let second = second.wait()?;
    ensure!(
        second.reply.as_deref() == Some(served_by(BuildLabel::Next, &g_next).as_str()),
        "the second root was not answered by N+1: {second:?}"
    );

    let drives = block_on(view.invocations("LashSession", &session, "drive"))?;
    let runs = block_on(view.invocations_like("LashTurn", &session, "run"))?;
    record(
        leg,
        "pinned-drive.json",
        &format!("drives {drives:?}\nruns {runs:?}"),
    )?;
    let pinned = drives.first().context("no drive ran")?;
    ensure!(
        pinned.pinned_deployment_id.as_deref() == Some(n_deployment),
        "the drive was not pinned to N: {drives:?}"
    );
    ensure!(runs.len() == 2, "expected two roots, ran {runs:?}");
    let (_, first_run) = &runs[0];
    let (_, second_run) = &runs[1];
    ensure!(
        first_run.pinned_deployment_id.as_deref() == Some(n_deployment),
        "the first root did not run on N: {runs:?}"
    );
    ensure!(
        second_run.pinned_deployment_id.as_deref() == Some(next_deployment.id.as_str())
            && second_run.invoked_by_id.as_deref() == Some(pinned.id.as_str()),
        "the second root was not admitted by the drive pinned to N and run on N+1: {runs:?}"
    );
    for marker in [first_marker.as_str(), second_marker] {
        let effects = case.effects_of(marker)?;
        ensure!(
            effects.len() == 1,
            "`{marker}` made {} model calls, not exactly one: {effects:?}",
            effects.len()
        );
    }
    Ok(next_node)
}

/// Start the signal-waiting process through `build`'s deployment and wait
/// until it waits for its signal. Answers its id.
fn start_waiting(case: &Case, build: &NodeBinary, key: &str) -> Result<String> {
    let started = build.start_process(case, key)?;
    let HarnessReply::Started {
        process_id,
        created: true,
        ..
    } = started.reply
    else {
        bail!("{key} did not start a process: {started:?}");
    };
    wait_for(&format!("{key} to wait for `{SIGNAL}`"), || {
        let status = build.process_status(case, &process_id)?;
        Ok((status.waiting_for.as_deref() == Some(SIGNAL)).then_some(()))
    })?;
    Ok(process_id)
}

/// Wait until `process` ends, and require that it ended with `payload`, the
/// one signal it recorded.
fn ended_with(
    case: &Case,
    build: &NodeBinary,
    process: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let status = wait_for(&format!("{process} to end"), || {
        let status = build.process_status(case, process)?;
        Ok(status.output.is_some().then_some(status))
    })?;
    ensure!(
        status.output.as_ref() == Some(payload) && status.signals == [payload.clone()],
        "{process} did not end with its one signal {payload}: {status:?}"
    );
    Ok(())
}

/// The segments `process` ran, by workflow key.
fn segments(view: &RestateView, process: &str) -> Result<Vec<ProcessSegment>> {
    block_on(view.process_segments(process))
}

/// The processes the race signals, started on N while N is the only
/// deployment: every root segment runs on N.
struct Racers {
    settled_first: String,
    at_the_drain: String,
    from_both_builds: String,
    after_the_successor: String,
}

fn start_racers(
    case: &Case,
    n: &NodeBinary,
    view: &RestateView,
    n_deployment: &str,
) -> Result<Racers> {
    let racers = Racers {
        settled_first: start_waiting(case, n, "settled-first")?,
        at_the_drain: start_waiting(case, n, "at-the-drain")?,
        from_both_builds: start_waiting(case, n, "from-both-builds")?,
        after_the_successor: start_waiting(case, n, "after-the-successor")?,
    };
    for process in [
        &racers.settled_first,
        &racers.at_the_drain,
        &racers.from_both_builds,
        &racers.after_the_successor,
    ] {
        let runs = segments(view, process)?;
        ensure!(
            runs.len() == 1
                && runs[0].key == *process
                && runs[0].invocation.pinned_deployment_id.as_deref() == Some(n_deployment),
            "{process}'s root segment did not run on N: {runs:?}"
        );
    }
    Ok(racers)
}

/// N's generation drains while its processes wait, and each signal is
/// delivered once: the process ends with it, records it once, and runs at
/// most one successor, on N+1.
fn signals_race_the_hand_off(
    leg: &Leg,
    case: &Case,
    view: &RestateView,
    racers: &Racers,
    deployments: (&str, &str),
    g_n: &str,
) -> Result<()> {
    let (n, next) = (&leg.builds.n, &leg.builds.next);
    let (n_deployment, next_deployment) = deployments;
    let operator = Operator::for_case(case, LASHCTL_NEXT_ENV)?;
    let payload = |name: &str| json!({ "process": name });

    // Settled before the drain: never handed over.
    n.signal_process(
        case,
        &racers.settled_first,
        "only",
        &payload("settled-first"),
    )?;
    ended_with(case, n, &racers.settled_first, &payload("settled-first"))?;

    operator.run("drain", Some(g_n))?;
    // At the drain, racing the leader's wake.
    next.signal_process(case, &racers.at_the_drain, "only", &payload("at-the-drain"))?;
    // From both builds at once, under one signal id.
    let from_n = n.spawn_signal(
        case,
        &racers.from_both_builds,
        "only",
        &payload("from-both-builds"),
    )?;
    let from_next = next.spawn_signal(
        case,
        &racers.from_both_builds,
        "only",
        &payload("from-both-builds"),
    )?;
    let both = [from_n.wait()?, from_next.wait()?];
    record(leg, "from-both-builds.json", &both)?;
    // After the hand-over, once the successor waits again on N+1.
    let process = &racers.after_the_successor;
    wait_for("the drain to hand the waiting process to N+1", || {
        let handed = segments(view, process)?
            .iter()
            .any(|segment| segment.key == format!("{process}#1"));
        Ok(handed.then_some(()))
    })?;
    let last = std::cell::RefCell::new(None);
    wait_for("the successor to wait again", || {
        let status = next.process_status(case, process)?;
        let waiting = status.waiting_for.as_deref() == Some(SIGNAL);
        last.replace(Some(status));
        Ok(waiting.then_some(()))
    })
    .with_context(|| {
        format!(
            "the process {:?}, its segments {:?}",
            last.borrow(),
            segments(view, process).ok()
        )
    })?;
    n.signal_process(case, process, "only", &payload("after-the-successor"))?;

    for (name, process) in [
        ("at-the-drain", &racers.at_the_drain),
        ("from-both-builds", &racers.from_both_builds),
        ("after-the-successor", &racers.after_the_successor),
    ] {
        ended_with(case, next, process, &payload(name))?;
    }

    let mut evidence = serde_json::Map::new();
    for (name, process) in [
        ("settled-first", &racers.settled_first),
        ("at-the-drain", &racers.at_the_drain),
        ("from-both-builds", &racers.from_both_builds),
        ("after-the-successor", &racers.after_the_successor),
    ] {
        let runs = segments(view, process)?;
        evidence.insert(name.to_owned(), json!(format!("{runs:?}")));
        let root = process.to_string();
        let successor = format!("{process}#1");
        let roots: Vec<_> = runs.iter().filter(|run| run.key == root).collect();
        let successors: Vec<_> = runs.iter().filter(|run| run.key == successor).collect();
        ensure!(
            roots.len() == 1
                && roots[0].invocation.pinned_deployment_id.as_deref() == Some(n_deployment),
            "{name}: the root segment did not run once on N: {runs:?}"
        );
        ensure!(
            successors.len() <= 1
                && successors
                    .iter()
                    .all(|run| run.invocation.pinned_deployment_id.as_deref()
                        == Some(next_deployment)),
            "{name}: a hand-over ran more than one successor, or one off N+1: {runs:?}"
        );
        ensure!(
            runs.iter()
                .all(|run| run.key == root || run.key == successor),
            "{name}: a successor handed over again: {runs:?}"
        );
        match name {
            "settled-first" => ensure!(
                successors.is_empty(),
                "{name}: a wait its signal settled first was handed over: {runs:?}"
            ),
            "after-the-successor" => ensure!(
                successors.len() == 1,
                "{name}: the drain never handed the wait over: {runs:?}"
            ),
            _ => {}
        }
    }
    record(leg, "signal-race.json", &evidence)?;

    let status = wait_for("N's generation to drain", || {
        let status = operator.run("drain-status", Some(g_n))?;
        Ok((status["result"]["drained"] == true || status["drained"] == true).then_some(status))
    })?;
    record(leg, "drain-status.json", &status)?;
    operator.run("end-drain", Some(g_n))?;
    Ok(())
}

/// A process N+1 started waits across the rollback in N+1's continuation
/// format. N signals it, and every segment it runs stays on N+1's
/// deployment, never on N's.
fn continuation_stays_on_next(
    leg: &Leg,
    case: &Case,
    view: &RestateView,
    next_deployment: &str,
    n_fresh_deployment: &str,
    process: &str,
) -> Result<()> {
    let n = &leg.builds.n;
    let payload = json!({ "process": "continuation" });
    let sent = n.signal_process(case, process, "only", &payload)?;
    ensure!(
        matches!(
            sent.reply,
            HarnessReply::Signalled {
                build: BuildLabel::N,
                ..
            }
        ),
        "N did not deliver the signal: {sent:?}"
    );
    ended_with(case, n, process, &payload)?;
    let runs = segments(view, process)?;
    record(leg, "continuation.json", &format!("{runs:?}"))?;
    ensure!(
        !runs.is_empty()
            && runs.iter().all(|run| {
                run.invocation.pinned_deployment_id.as_deref() == Some(next_deployment)
            }),
        "a segment of N+1's process left N+1's deployment: {runs:?}"
    );
    ensure!(
        runs.iter().all(|run| {
            run.invocation.pinned_deployment_id.as_deref() != Some(n_fresh_deployment)
        }),
        "N ran a segment of N+1's process: {runs:?}"
    );
    Ok(())
}

/// A foreign-`G` journal dispatches zero effects and parks. Signals that
/// race a hand-off are delivered exactly once. A root admitted by a drive
/// pinned to N runs on N+1, with no refusal and no `SubstrateLost`. A
/// continuation N cannot decode keeps N+1's deployment and routes there.
/// Rollback registers N at a fresh URI. Registering at a URI that serves
/// another generation is refused.
#[test]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just phase-a` runs it"]
fn generation_handoff_rollback() -> Result<()> {
    let leg = Leg::start("generation_handoff_rollback")?;
    let (n, next) = (&leg.builds.n, &leg.builds.next);
    let case = Case::postgres_database("handoff", &leg.services, &leg.scratch)?;
    Operator::for_case(&case, LASHCTL_N_ENV)?.run("migrate", None)?;
    Operator::for_case(&case, LASHCTL_NEXT_ENV)?.run("migrate", None)?;
    let view = case.view()?;

    let n_first = n.serve(&case)?;
    let g_n = n_first.generation()?.to_owned();
    let n_uri = n_first.uri()?.to_owned();
    let n_deployment = block_on(view.deployment_at(&n_uri))?;

    let n_restored =
        foreign_journal_parks(&leg, &case, &view, n_first).context("the foreign journal")?;
    let racers = start_racers(&case, n, &view, &n_deployment.id).context("the racers' start")?;
    let next_node = pinned_drive_root(&leg, &case, &view, &n_deployment.id, &g_n)
        .context("the pinned drive")?;
    let g_next = next_node.generation()?.to_owned();
    let next_deployment = block_on(view.deployment_at(next_node.uri()?))?;
    signals_race_the_hand_off(
        &leg,
        &case,
        &view,
        &racers,
        (&n_deployment.id, &next_deployment.id),
        &g_n,
    )
    .context("the signal race")?;

    // N+1 may not take N's URI.
    let before = block_on(view.deployments())?;
    let refused = next.register(&case, &n_uri)?;
    record(&leg, "registration-refused.json", &refused)?;
    ensure!(
        refused.registered
            == Err(EndpointServesAnotherGeneration {
                uri: n_uri.clone(),
                held: Some(g_n.clone()),
                local: g_next.clone(),
            }),
        "N+1 registering at N's URI answered {:?}",
        refused.registered
    );
    let after = block_on(view.deployments())?;
    ensure!(
        before == after,
        "a refused registration changed the deployments"
    );

    // A process N+1 starts while it is the newest deployment waits across
    // the rollback.
    let continuation = start_waiting(&case, next, "continuation")?;

    // Rollback: N registers at a fresh URI, beside N+1's deployment.
    let n_fresh = n.serve(&case)?;
    ensure!(
        n_fresh.uri()? != n_uri && n_fresh.uri()? != next_node.uri()?,
        "the rollback reused a URI"
    );
    let fresh = block_on(view.deployment_at(n_fresh.uri()?))?;
    let deployments = block_on(view.deployments())?;
    ensure!(
        fresh.id != n_deployment.id
            && deployments
                .iter()
                .any(|deployment| deployment.id == next_deployment.id),
        "the rollback did not register N beside N+1: {deployments:?}"
    );
    let turn = n.turn(
        &case,
        &case.session_id("after-rollback"),
        "a turn after the rollback",
    )?;
    ensure!(
        turn.reply.as_deref() == Some(served_by(BuildLabel::N, &g_n).as_str()),
        "N did not answer after the rollback: {turn:?}"
    );
    continuation_stays_on_next(
        &leg,
        &case,
        &view,
        &next_deployment.id,
        &fresh.id,
        &continuation,
    )
    .context("the continuation")?;

    n_fresh.stop()?;
    next_node.stop()?;
    n_restored.stop()?;
    Ok(())
}
