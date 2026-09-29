//! `object_sweep_crash_resume` (ADR 0115 §6): Restate object state across
//! a roll, a finalize and the synthetic N+1's sweep.
//!
//! Before finalize, N and then N+1 (the newest deployment) each open effect
//! groups and answer a turn. Every group's `_compat` and stored value, and
//! every `LashTurn` outcome, is at N's format: the fleet epoch pins N+1's
//! writer to it. After a rollback N's handlers read every one.
//!
//! Then the fleet drains N's generation and finalizes (the synthetic
//! finalize moves the recorded epoch; `lashctl finalize` is FIG-3800 B's).
//! N+1, restarted to read the new epoch, sweeps: its `upgrade` handler
//! rewrites each group at format 2 and raises its `_compat`. The sweep and
//! N+1's deployment are killed mid-sweep; the synthetic preflight lists the
//! groups still at format 1, N+1 comes back at the crashed deployment's
//! URI, and a second sweep finishes exactly the groups the preflight
//! listed. Finally an operator keeps an N deployment by registering it: its
//! handlers are refused by every swept group's `_compat`, typed, with no
//! state changed.

use anyhow::{Context, Result, ensure};
use lash_core_store::compat::CompatRefusal;
use lash_restate::{COMPAT_KEY, LASH_TURN_OUTCOME_FORMAT_VERSION, ObjectCompat, VersionRange};
use lash_upgrade_harness::harness::{
    CallSpec, Case, LASHCTL_N_ENV, LASHCTL_NEXT_ENV, NodeBinary, Operator, ServeOptions, block_on,
};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::objects::{CallOutcome, HandlerRefusal};
use lash_upgrade_harness::node::served_by;
use lash_upgrade_harness::restate_view::RestateView;

use crate::support::{GROUP, Leg, open_group_body, record, refused, replied};

/// Each build opens this many groups before finalize.
const GROUPS_PER_BUILD: usize = 3;
/// The sweep is crashed after this many groups.
const SWEPT_BEFORE_CRASH: usize = 2;
/// The state key a group's record is stored under.
const GROUP_STATE_KEY: &str = "effect-group/v1/state";

/// Open `count` fresh groups as a caller of `build`, each answered at
/// `wire`.
fn open_groups(
    case: &Case,
    view: &RestateView,
    build: &NodeBinary,
    prefix: &str,
    count: usize,
    wire: u32,
) -> Result<Vec<String>> {
    (0..count)
        .map(|index| {
            let key = case.session_id(&format!("{prefix}-{index}"));
            let opened = build.call(
                case,
                &CallSpec::object(GROUP, &key, "open").body(open_group_body(view, &key)?),
            )?;
            let (answered, body) = replied(&opened)?;
            ensure!(
                answered == wire && body["type"] == "opened_fresh",
                "opening {key} answered {answered} {body}"
            );
            Ok(key)
        })
        .collect()
}

/// A group's `_compat` and the format its record is stamped with.
fn group_formats(view: &RestateView, key: &str) -> Result<(ObjectCompat, u64)> {
    let state = block_on(view.object_state(GROUP, key))?;
    let compat: ObjectCompat = serde_json::from_value(
        state
            .get(COMPAT_KEY)
            .cloned()
            .with_context(|| format!("{key} holds no `_compat`: {state:?}"))?,
    )?;
    let format = state
        .get(GROUP_STATE_KEY)
        .and_then(|value| value["format"].as_u64())
        .with_context(|| format!("{key} holds no stamped record: {state:?}"))?;
    Ok((compat, format))
}

/// The `LashTurn` outcomes recorded for `session`, by workflow key.
fn outcomes_of(view: &RestateView, session: &str) -> Result<Vec<(String, serde_json::Value)>> {
    Ok(block_on(view.values_named("LashTurn", "outcome"))?
        .into_iter()
        .filter(|(key, _)| key.contains(session))
        .collect())
}

/// Every group is admitted by N+1 and holds a record at `format`.
fn all_at(view: &RestateView, groups: &[String], format: u32) -> Result<()> {
    for key in groups {
        let (compat, stored) = group_formats(view, key)?;
        ensure!(
            compat == ObjectCompat::fresh(format) && stored == u64::from(format),
            "{key} is at `_compat` {compat:?} with a record at {stored}, not {format}"
        );
    }
    Ok(())
}

/// Objects and `LashTurn` outcomes written by N and by N+1 before finalize
/// are all in N's format, and N reads them all. After finalize the
/// synthetic sweep converts objects and survives a crash mid-sweep.
/// Preflight lists the objects still at format 1. A kept N handler is
/// refused typed by `_compat`.
#[test]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just phase-a` runs it"]
fn object_sweep_crash_resume() -> Result<()> {
    let leg = Leg::start("object_sweep_crash_resume")?;
    let (n, next) = (&leg.builds.n, &leg.builds.next);
    let case = Case::postgres_database("sweep", &leg.services, &leg.scratch)?;
    let operator_n = Operator::for_case(&case, LASHCTL_N_ENV)?;
    let operator_next = Operator::for_case(&case, LASHCTL_NEXT_ENV)?;
    let view = case.view()?;
    operator_n.run("migrate", None)?;

    // Before finalize: N writes, then N+1 writes as the newest deployment.
    let n_first = n.serve(&case)?;
    let n_generation = n_first.generation()?.to_owned();
    let mut groups = open_groups(&case, &view, n, "sweep-n", GROUPS_PER_BUILD, 1)?;
    let session_n = case.session_id("sweep-turn-n");
    let turn = n.turn(&case, &session_n, "a turn N answers")?;
    ensure!(turn.reply.as_deref() == Some(served_by(BuildLabel::N, &n_generation).as_str()));

    operator_next.run("migrate", None)?;
    let next_first = next.serve(&case)?;
    let next_generation = next_first.generation()?.to_owned();
    let next_deployment = block_on(view.deployment_at(next_first.uri()?))?;
    let written_by_next = open_groups(&case, &view, next, "sweep-next", GROUPS_PER_BUILD, 2)?;
    for key in &written_by_next {
        let invocations = block_on(view.invocations(GROUP, key, "open"))?;
        ensure!(
            invocations.len() == 1
                && invocations[0].pinned_deployment_id.as_deref()
                    == Some(next_deployment.id.as_str()),
            "{key} was not opened by N+1's handler: {invocations:?}"
        );
    }
    groups.extend(written_by_next);
    let session_next = case.session_id("sweep-turn-next");
    let turn = next.turn(&case, &session_next, "a turn N+1 answers")?;
    ensure!(
        turn.reply.as_deref() == Some(served_by(BuildLabel::Next, &next_generation).as_str()),
        "N+1 did not drive its turn: {turn:?}"
    );

    // Everything either build wrote is in N's format.
    all_at(&view, &groups, 1).context("before finalize")?;
    let mut outcomes = outcomes_of(&view, &session_n)?;
    outcomes.extend(outcomes_of(&view, &session_next)?);
    ensure!(
        outcomes.len() == 2,
        "expected one outcome per turn, found {outcomes:?}"
    );
    for (key, outcome) in &outcomes {
        ensure!(
            outcome["format"] == LASH_TURN_OUTCOME_FORMAT_VERSION,
            "{key}'s outcome is stamped {}",
            outcome["format"]
        );
    }

    // Rollback: N, at a fresh URI, reads every group and every outcome.
    let n_back = n.serve(&case)?;
    let n_back_deployment = block_on(view.deployment_at(n_back.uri()?))?;
    for key in &groups {
        let probe = n.call(&case, &CallSpec::object(GROUP, key, "probe"))?;
        let (wire, body) = replied(&probe)?;
        ensure!(
            wire == 1 && body["type"] == "exists",
            "N read {key} as {wire} {body}"
        );
        let invocations = block_on(view.invocations(GROUP, key, "probe"))?;
        ensure!(
            invocations
                .last()
                .and_then(|invocation| invocation.pinned_deployment_id.as_deref())
                == Some(n_back_deployment.id.as_str()),
            "N's probe of {key} did not run on N: {invocations:?}"
        );
    }
    for (key, _) in &outcomes {
        let read = n.call(&case, &CallSpec::workflow("LashTurn", key, "outcome"))?;
        let (wire, body) = replied(&read)?;
        ensure!(
            wire == 1 && !body.is_null(),
            "N read {key}'s outcome as {wire} {body}"
        );
    }

    // An N deployment an operator will keep: it opened the store at N's
    // epoch, and serves unregistered for now.
    let kept_n = n.serve_with(
        &case,
        &ServeOptions {
            unregistered: true,
            register_later: true,
            ..ServeOptions::default()
        },
    )?;

    // Roll forward, drain N's generation, and finalize.
    let next_rolled = next.serve(&case)?;
    operator_next.run("drain", Some(&n_generation))?;
    n_first.stop()?;
    n_back.stop()?;
    let status = operator_next.run("drain-status", Some(&n_generation))?;
    ensure!(status["drained"] == true, "N did not drain: {status}");
    let finalized = case.retire_and_finalize(&operator_next, &n_generation)?;
    ensure!(
        finalized["fleet_format"] == 2,
        "finalize did not move F: {finalized}"
    );
    operator_next.run("end-drain", Some(&n_generation))?;
    // N+1 comes back to read the finalized epoch.
    next_rolled.stop()?;
    next_first.stop()?;
    let next_final = next.serve(&case)?;

    // Preflight: every group is still at format 1.
    let mut pending = block_on(view.objects_at_format(GROUP, 1))?;
    pending.sort();
    let mut expected = groups.clone();
    expected.sort();
    ensure!(
        pending == expected,
        "preflight listed {pending:?}, not {expected:?}"
    );

    // Sweep, and crash the sweep and N+1 mid-sweep.
    let mut sweeper = next.spawn_sweep(&case)?;
    let mut swept = Vec::new();
    for _ in 0..SWEPT_BEFORE_CRASH {
        let line = sweeper.next_object()?.context("the sweep ended early")?;
        match &line.outcome {
            CallOutcome::Replied { wire: 2, body }
                if body["upgrade"] == "upgraded" && body["from"] == 1 && body["format"] == 2 => {}
            other => anyhow::bail!("the sweep answered {} with {other:?}", line.key),
        }
        swept.push(line.key);
    }
    let bind = next_final.bind()?;
    next_final.stop()?;
    sweeper.crash()?;

    // Preflight lists what is left; the swept groups are at format 2.
    let left = block_on(view.objects_at_format(GROUP, 1))?;
    ensure!(
        swept.iter().all(|key| !left.contains(key)),
        "preflight lists a swept group: {left:?}"
    );
    ensure!(
        left.len() >= groups.len() - SWEPT_BEFORE_CRASH - 1
            && left.len() <= groups.len() - SWEPT_BEFORE_CRASH,
        "preflight lists {} groups after {SWEPT_BEFORE_CRASH} of {} were swept: {left:?}",
        left.len(),
        groups.len()
    );
    all_at(&view, &swept, 2).context("the groups swept before the crash")?;

    // N+1 comes back at the crashed deployment's URI, and a second sweep
    // finishes the rest.
    let next_back = next.serve_with(
        &case,
        &ServeOptions {
            bind: Some(bind),
            ..ServeOptions::default()
        },
    )?;
    let resumed = next.spawn_sweep(&case)?.finish()?;
    record(&leg, "sweep-resumed.json", &resumed)?;
    for line in &resumed {
        match &line.outcome {
            CallOutcome::Replied { wire: 2, body }
                if body["upgrade"] == "upgraded" || body["upgrade"] == "current" => {}
            other => anyhow::bail!("the resumed sweep answered {} with {other:?}", line.key),
        }
    }
    ensure!(
        resumed.iter().all(|line| !swept.contains(&line.key)),
        "the resumed sweep visited a group swept before the crash"
    );
    let left = block_on(view.objects_at_format(GROUP, 1))?;
    ensure!(left.is_empty(), "preflight still lists {left:?}");
    all_at(&view, &groups, 2).context("after the sweep")?;
    for key in &groups {
        let probe = next.call(&case, &CallSpec::object(GROUP, key, "probe"))?;
        let (wire, body) = replied(&probe)?;
        ensure!(
            wire == 2 && body["type"] == "exists",
            "N+1 read {key} as {wire} {body}"
        );
    }

    // An operator keeps the N deployment: its handlers are refused by
    // `_compat`, typed, and change nothing.
    kept_n.register_now()?;
    let kept = block_on(view.deployment_at(kept_n.uri()?))?;
    let key = &groups[0];
    let before = block_on(view.object_state(GROUP, key))?;
    let floor = CompatRefusal::ReaderFloorAbove {
        component: "restate-effect-group-state".to_owned(),
        found: 2,
        min_reader: 2,
        reads: VersionRange::exactly(1),
        writing_release: None,
    };
    for (handler, body) in [
        ("probe", serde_json::Value::Null),
        ("open", open_group_body(&view, key)?),
    ] {
        let refusal = next.call(&case, &CallSpec::object(GROUP, key, handler).body(body))?;
        record(&leg, &format!("kept-n-{handler}.json"), &refusal)?;
        ensure!(
            *refused(&refusal)?
                == HandlerRefusal::Incompatible {
                    refusal: floor.clone(),
                },
            "the kept N {handler} answered {:?}",
            refusal.outcome
        );
        let invocations = block_on(view.invocations(GROUP, key, handler))?;
        ensure!(
            invocations
                .last()
                .and_then(|invocation| invocation.pinned_deployment_id.as_deref())
                == Some(kept.id.as_str()),
            "the refused {handler} did not run on the kept N deployment: {invocations:?}"
        );
    }
    let after = block_on(view.object_state(GROUP, key))?;
    ensure!(
        before == after,
        "a refused handler changed {key}: {before:?} to {after:?}"
    );

    kept_n.stop()?;
    next_back.stop()?;
    Ok(())
}
