//! `object_sweep_crash_resume` (ADR 0115 §6): Restate object state across
//! a roll, a finalize and the synthetic N+1's sweep.
//!
//! Before finalize, N and then N+1 (the newest deployment) each open effect
//! groups and answer a turn. Every group's `_compat` and stored value, and
//! every `LashTurn` outcome, is at N's format: the fleet epoch pins N+1's
//! writer to it. After a rollback N's handlers read every one.
//!
//! Before finalize, `lashctl objects-sweep` is refused `not_finalized` by
//! N+1's `upgrade` handlers, with nothing rewritten. Then the fleet drains
//! N's generation and finalizes with `lashctl finalize`. N+1, restarted to
//! read the new epoch, sweeps with lash's object sweep (FIG-4041): every
//! family's `upgrade` handler rewrites each object at format 2 and raises its
//! `_compat`. The sweep and N+1's deployment are killed mid-sweep; `lashctl
//! objects-preflight` lists the objects still at format 1, N+1 comes back at
//! the crashed deployment's URI, and `lashctl objects-sweep` finishes exactly
//! the objects the preflight listed. Finally an operator keeps an N
//! deployment by registering it: its handlers are refused by every swept
//! group's `_compat`, typed, with no state changed.

use anyhow::{Context, Result, ensure};
use lash_core_store::compat::CompatRefusal;
use lash_restate::ObjectUpgradeResponse;
use lash_restate::{COMPAT_KEY, LASH_TURN_OUTCOME_FORMAT_VERSION, ObjectCompat, VersionRange};
use lash_upgrade_harness::harness::{
    CallSpec, Case, LASHCTL_N_ENV, LASHCTL_NEXT_ENV, NodeBinary, Operator, ServeOptions, block_on,
};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::objects::HandlerRefusal;
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

/// The keys `lashctl objects-preflight`'s `--json` body lists for
/// `service`, sorted.
fn preflight_keys(body: &serde_json::Value, service: &str) -> Vec<String> {
    let mut keys = body["result"]["families"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|family| family["service"] == service)
        .flat_map(|family| family["pending"].as_array().cloned().unwrap_or_default())
        .filter_map(|pending| pending["key"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    keys.sort();
    keys
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
    let mut kept_n = n.serve_with(
        &case,
        &ServeOptions {
            unregistered: true,
            register_later: true,
            ..ServeOptions::default()
        },
    )?;

    // Roll forward. Before finalize the sweep is refused at its first
    // object by N+1's `upgrade`, and nothing is rewritten.
    let next_rolled = next.serve(&case)?;
    let (code, refused_sweep) = operator_next.answer_args(&case.objects_sweep_args())?;
    record(&leg, "sweep-before-finalize.json", &refused_sweep)?;
    ensure!(
        code == 3 && refused_sweep["error"]["refusal"]["refusal"] == "not_finalized",
        "the sweep before finalize answered {code} {refused_sweep}"
    );
    all_at(&view, &groups, 1).context("after the refused sweep")?;

    // Drain N's generation, and finalize.
    operator_next.run("drain", Some(&n_generation))?;
    n_first.stop()?;
    n_back.stop()?;
    let status = operator_next.run_args(&case.drain_status_args(&n_generation))?;
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

    // Preflight: every group is still at format 1, and `lashctl
    // objects-preflight` lists them, with the other families' objects.
    let mut pending = block_on(view.objects_at_format(GROUP, 1))?;
    pending.sort();
    let mut expected = groups.clone();
    expected.sort();
    ensure!(
        pending == expected,
        "the state lists {pending:?}, not {expected:?}"
    );
    let (code, preflight) = operator_next.answer_args(&case.objects_preflight_args())?;
    record(&leg, "preflight-before-sweep.json", &preflight)?;
    ensure!(
        code == 5 && preflight["result"]["upgraded"] == false,
        "the preflight answered {code} {preflight}"
    );
    ensure!(
        preflight_keys(&preflight, GROUP) == expected,
        "the preflight listed {preflight}, not {expected:?}"
    );

    // Sweep, and crash the sweep and N+1 mid-sweep.
    let mut pending_objects = std::collections::BTreeSet::new();
    for family in preflight["result"]["families"]
        .as_array()
        .context("the preflight lists its families")?
    {
        let service = family["service"]
            .as_str()
            .context("the preflight family names its service")?;
        for object in family["pending"]
            .as_array()
            .context("the preflight family lists its pending objects")?
        {
            let key = object["key"]
                .as_str()
                .context("the preflight object names its key")?;
            ensure!(
                object["format"] == 1
                    && pending_objects.insert((service.to_owned(), key.to_owned())),
                "unexpected preflight object in {service}: {object}"
            );
        }
    }
    let mut sweeper = next.spawn_sweep(&case)?;
    let mut visited = Vec::new();
    let mut swept = Vec::new();
    while swept.len() < SWEPT_BEFORE_CRASH {
        let line = sweeper.next_object()?.context("the sweep ended early")?;
        ensure!(
            pending_objects.remove(&(line.service.clone(), line.key.clone()))
                && line.outcome == ObjectUpgradeResponse::Upgraded { from: 1, format: 2 },
            "the sweep answered {line:?}"
        );
        if line.service == GROUP {
            swept.push(line.key.clone());
        }
        visited.push(line);
    }
    record(&leg, "swept-before-crash.json", &visited)?;
    let bind = next_final.bind()?;
    next_final.stop()?;
    sweeper.crash()?;

    // The preflight lists what is left; the swept groups are at format 2.
    let left = block_on(view.objects_at_format(GROUP, 1))?;
    ensure!(
        swept.iter().all(|key| !left.contains(key)),
        "the state lists a swept group: {left:?}"
    );
    ensure!(
        left.len() >= groups.len() - SWEPT_BEFORE_CRASH - 1
            && left.len() <= groups.len() - SWEPT_BEFORE_CRASH,
        "{} groups are left after {SWEPT_BEFORE_CRASH} of {} were swept: {left:?}",
        left.len(),
        groups.len()
    );
    let (code, preflight) = operator_next.answer_args(&case.objects_preflight_args())?;
    record(&leg, "preflight-after-crash.json", &preflight)?;
    let mut left_sorted = left.clone();
    left_sorted.sort();
    ensure!(
        code == 5 && preflight_keys(&preflight, GROUP) == left_sorted,
        "the preflight after the crash answered {code} {preflight}, not {left_sorted:?}"
    );
    all_at(&view, &swept, 2).context("the groups swept before the crash")?;

    // N+1 comes back at the crashed deployment's URI, and `lashctl
    // objects-sweep` finishes the rest.
    let next_back = next.serve_with(
        &case,
        &ServeOptions {
            bind: Some(bind),
            ..ServeOptions::default()
        },
    )?;
    let resumed = operator_next.run_args(&case.objects_sweep_args())?;
    record(&leg, "sweep-resumed.json", &resumed)?;
    let resumed_lines = resumed["swept"]
        .as_array()
        .context("the sweep lists what it swept")?;
    for line in resumed_lines {
        ensure!(
            line["outcome"]["upgrade"] == "upgraded" || line["outcome"]["upgrade"] == "current",
            "the resumed sweep answered {line}"
        );
        ensure!(
            line["service"] != GROUP || !swept.iter().any(|key| line["key"] == key.as_str()),
            "the resumed sweep visited a group swept before the crash: {line}"
        );
    }
    ensure!(
        resumed["remaining"].as_array().is_some_and(Vec::is_empty),
        "the resumed sweep left {resumed}"
    );
    let left = block_on(view.objects_at_format(GROUP, 1))?;
    ensure!(left.is_empty(), "the state still lists {left:?}");
    let preflight = operator_next.run_args(&case.objects_preflight_args())?;
    ensure!(
        preflight["upgraded"] == true,
        "the preflight after the sweep lists {preflight}"
    );
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
    let kept_uri = kept_n.uri()?.to_owned();
    let registered = kept_n.register(&kept_uri)?;
    ensure!(
        registered.registered == Ok(()),
        "N's registration: {registered:?}"
    );
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
