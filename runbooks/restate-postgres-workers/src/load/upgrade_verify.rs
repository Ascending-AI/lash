//! The classes of a load run under the rolling-upgrade campaign (FIG-3805
//! phase B, `scripts/loadtest_upgrade.py`). The controller runs the ADR 0106
//! §6 choreography, N and the synthetic N+1 side by side, while the load's
//! sessions keep sending, and records each step in the witness ledger. The
//! laws, read against the witness clock:
//!
//! - **No lost or duplicated effects across the roll.** Every step passes
//!   the fault checks: the operations in flight at it reached durable
//!   answers and service went on. The run's `turns`, `tools` and
//!   `child-processes` classes span the whole roll, so every answered
//!   turn's effects committed exactly once with the regenerated result and
//!   no effect committed that no turn planned.
//! - **Every session keeps working through the roll** (`sessions-through-roll`):
//!   every session that had sent a turn before a step settled a turn it
//!   sent after that step: answered, or cancelled as its plan asked.
//! - **The rollback restores N cleanly before finalize** (`rollback`): N
//!   serves again at its own generation, takes new admission, N+1's
//!   generation drains with nothing pinned or stalled and retires, and the
//!   rollback lands before finalize.
//! - **Stale writers are fenced after finalize** (`finalize`, `fence`):
//!   finalize moved `F` from 1 to 2 only after N's deployments were
//!   removed, and afterwards the running N writer's write was refused
//!   `WriterFenced` and wrote nothing, a fresh N process refused the store,
//!   and N's operator refused it.

use super::fault_verify::{Timeline, campaign_outcome, moved_to, timelines, verify_injection};
use super::verify::WitnessSnapshot;
use lash_perf::workload::OperationId;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// The steps, in the order the campaign must run them.
const UPGRADE_STEPS: [&str; 5] = ["half-roll", "rollback", "roll", "finalize", "fence"];

/// The sessions (load actors) that sent a turn before `at`.
fn sessions_before(timelines: &[Timeline<'_>], at: i64) -> BTreeSet<u64> {
    timelines
        .iter()
        .filter(|timeline| timeline.operation == "turn" && timeline.sent_at < at)
        .filter_map(|timeline| OperationId::parse(timeline.subject).ok())
        .map(|(id, _)| id.actor)
        .collect()
}

/// Whether session `actor` settled a turn it sent after `at`: answered it,
/// or cancelled it as its plan asked (the `turns` class holds each turn to
/// its plan).
fn settled_after_by(timelines: &[Timeline<'_>], at: i64, actor: u64) -> bool {
    timelines.iter().any(|timeline| {
        timeline.operation == "turn"
            && timeline.sent_at > at
            && OperationId::parse(timeline.subject).is_ok_and(|(id, _)| id.actor == actor)
            && timeline.response().is_some_and(|response| {
                matches!(
                    response["outcome"]["status"].as_str(),
                    Some("answered" | "cancelled")
                )
            })
    })
}

fn drained(recovery: &Value) -> bool {
    recovery["drained"] == true
        && recovery["pinned_unfinished"] == 0
        && recovery["stalled_total"] == 0
}

/// The rolling-upgrade classes: the campaign, each step and the sessions
/// law. A step with no recorded injection has no evidence, so a campaign
/// that skipped one fails its class.
pub(super) fn verify_upgrade(
    snapshot: &WitnessSnapshot,
    note: &mut impl FnMut(&'static str, Result<(), String>),
) {
    let timelines = timelines(snapshot);
    note("upgrade-campaign", campaign_outcome(snapshot));
    for row in &snapshot.faults {
        if row.kind != "campaign" && !UPGRADE_STEPS.contains(&row.kind.as_str()) {
            note(
                "upgrade-campaign",
                Err(format!(
                    "step `{}` has unknown kind `{}`",
                    row.fault_id, row.kind
                )),
            );
        }
    }
    // Each build's drain generation, as the half roll recorded them.
    let mut generations: Option<(Value, Value)> = None;
    let mut previous: Option<(&str, i64)> = None;
    for step in UPGRADE_STEPS {
        let Some((injected, recovered)) =
            verify_injection(step, step, step, snapshot, &timelines, note)
        else {
            continue;
        };
        let at = injected.recorded_at_us;
        if let Some((before, recovered_at)) = previous
            && at < recovered_at
        {
            note(
                step,
                Err(format!("{step} was injected before {before} recovered")),
            );
        }
        previous = Some((step, recovered.recorded_at_us));

        let sessions = sessions_before(&timelines, at);
        let silent: Vec<u64> = sessions
            .iter()
            .copied()
            .filter(|actor| !settled_after_by(&timelines, at, *actor))
            .collect();
        note(
            "sessions-through-roll",
            if sessions.is_empty() {
                Err(format!("no session had sent a turn before {step}"))
            } else if silent.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "sessions {silent:?} settled no turn sent after {step}"
                ))
            },
        );

        let (i, r) = (&injected.detail, &recovered.detail);
        let (n, next) = generations.clone().unwrap_or((Value::Null, Value::Null));
        let held = match step {
            // N+1 migrated the store, preflighted it and took admission
            // beside N; the forward drain of N's generation started.
            "half-roll" => {
                generations = Some((i["old_generation"].clone(), i["new_generation"].clone()));
                i["old_generation"].is_string()
                    && i["old_generation"] != i["new_generation"]
                    && i["migrate"].is_object()
                    && i["preflight"].is_object()
                    && r["forward_drain"] == i["old_generation"]
                    && moved_to(&timelines, at, &i["new_workers"])
            }
            // N came back at its own generation and took admission, and
            // N+1's generation drained and retired, before finalize.
            "rollback" => {
                i["restored_generation"] == n
                    && i["old_generation"] == next
                    && i["preflight"].is_object()
                    && drained(r)
                    && r["retired"] == "next"
                    && r["pinned_after_retirement"] == 0
                    && moved_to(&timelines, at, &i["new_workers"])
            }
            // N+1 took admission again and N's generation drained.
            "roll" => {
                i["old_generation"] == n
                    && i["new_generation"] == next
                    && i["migrate"].is_object()
                    && drained(r)
                    && moved_to(&timelines, at, &i["new_workers"])
            }
            // Finalize refused while N was registered, then moved F from 1
            // to 2 with every backfill applied; contract ran and the object
            // sweep left nothing at N's format.
            "finalize" => {
                i["retired_generation"] == n
                    && i["refused"] == "deployments_retained"
                    && r["finalized"] == true
                    && r["flip"] == json!({"outcome": "finalized", "from": 1, "to": 2})
                    && r["backfills"].as_array().is_some_and(|steps| {
                        !steps.is_empty() && steps.iter().all(|step| step["state"] == "applied")
                    })
                    && r["contract"]["executed"]
                        .as_array()
                        .is_some_and(|steps| !steps.is_empty())
                    && r["objects_upgraded"] == true
                    && r["objects_sweep"]["remaining"]
                        .as_array()
                        .is_some_and(Vec::is_empty)
            }
            // The running N writer was fenced and wrote nothing, a fresh N
            // process and N's operator refused the store.
            _ => {
                i["stale_generation"] == n
                    && i["live_write"]["status"]
                        .as_u64()
                        .is_some_and(|status| status >= 500)
                    && i["live_write"]["body"]
                        .as_str()
                        .is_some_and(|body| body.contains("writer fenced"))
                    && r["live_writer_fenced"] == true
                    && r["drain_mark_written"] == false
                    && r["fresh_open_refused"] == true
                    && r["fresh_open"]["exit"]
                        .as_i64()
                        .is_some_and(|code| code != 0)
                    && matches!(r["operator_preflight"]["exit"].as_i64(), Some(3 | 4))
            }
        };
        note(
            step,
            if held {
                Ok(())
            } else {
                Err(format!(
                    "{step} evidence does not hold: injected {i} recovered {r}"
                ))
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::verify::tests::{ideal, smoke, verdict, violated};
    use super::super::verify::{CLASSES, FaultRow, UPGRADE_CLASSES, WitnessSnapshot};
    use super::UPGRADE_STEPS;
    use crate::load::LoadContext;
    use lash_perf::workload::OperationId;
    use serde_json::{Value, json};

    fn row(fault_id: &str, kind: &str, phase: &str, detail: Value, at: i64) -> FaultRow {
        FaultRow {
            fault_id: fault_id.to_owned(),
            kind: kind.to_owned(),
            phase: phase.to_owned(),
            target: format!("{kind}-target"),
            detail,
            recorded_at_us: at,
        }
    }

    /// Each step's detail, as the controller records a correct run.
    fn details(step: &str, workers: &[&str]) -> (Value, Value) {
        let applied = json!([{ "state": "applied" }]);
        match step {
            "half-roll" => (
                json!({ "old_generation": "g0", "new_generation": "g1", "migrate": {}, "preflight": {},
                        "new_workers": workers }),
                json!({ "forward_drain": "g0" }),
            ),
            "rollback" => (
                json!({ "restored_generation": "g0", "old_generation": "g1", "preflight": {},
                        "new_workers": workers }),
                json!({ "drained": true, "pinned_unfinished": 0, "stalled_total": 0, "retired": "next",
                        "pinned_after_retirement": 0 }),
            ),
            "roll" => (
                json!({ "old_generation": "g0", "new_generation": "g1", "migrate": {}, "new_workers": workers }),
                json!({ "drained": true, "pinned_unfinished": 0, "stalled_total": 0 }),
            ),
            "finalize" => (
                json!({ "retired_generation": "g0", "refused": "deployments_retained" }),
                json!({ "finalized": true, "flip": { "outcome": "finalized", "from": 1, "to": 2 },
                        "backfills": applied, "contract": { "executed": [{}] }, "objects_upgraded": true,
                        "objects_sweep": { "remaining": [] } }),
            ),
            _ => (
                json!({ "stale_generation": "g0",
                        "live_write": { "status": 500, "body": "writer fenced: the fleet epoch is 2" } }),
                json!({ "live_writer_fenced": true, "drain_mark_written": false, "fresh_open_refused": true,
                        "fresh_open": { "exit": 1 }, "operator_preflight": { "exit": 4 } }),
            ),
        }
    }

    /// A correct smoke run under the campaign. The ideal run lists each
    /// session's turns in turn, so it is re-clocked round by round (every
    /// session's first turn, then every second turn, ...), as open-loop
    /// sessions interleave; each step then lands inside session 0's turn of
    /// its round, and every session settles a later turn.
    fn campaign(load: &LoadContext) -> WitnessSnapshot {
        let mut snapshot = ideal(load, lash_perf::workload::SMOKE_TURNS_PER_SESSION);
        let turn_key = |subject: &str| {
            let (id, _) = OperationId::parse(subject).expect("a turn key");
            (id.ordinal, id.actor)
        };
        // A turn's attachments precede it and its delete follows it; cron
        // runs after every round.
        let mut keys = vec![(u64::MAX, u64::MAX); snapshot.events.len()];
        let mut previous = None;
        for (index, event) in snapshot.events.iter().enumerate() {
            if event.operation == "turn" {
                previous = Some(turn_key(&event.subject));
            }
            if event.operation == "turn" || event.operation == "delete-session" {
                keys[index] = previous.expect("a turn precedes its delete");
            }
        }
        let mut next = None;
        for (index, event) in snapshot.events.iter().enumerate().rev() {
            if event.operation == "turn" {
                next = Some(turn_key(&event.subject));
            } else if event.operation == "attachment" {
                keys[index] = next.expect("a turn follows its attachments");
            }
        }
        let mut order: Vec<usize> = (0..snapshot.events.len()).collect();
        order.sort_by_key(|index| keys[*index]);
        let events = std::mem::take(&mut snapshot.events);
        snapshot.events = order
            .into_iter()
            .map(|index| events[index].clone())
            .collect();
        for (index, event) in snapshot.events.iter_mut().enumerate() {
            event.recorded_at_us = index as i64 * 10;
        }
        let end = snapshot.events.len() as i64 * 10;
        let workers: Vec<String> = snapshot
            .events
            .iter()
            .filter_map(|event| event.detail["response"]["worker_id"].as_str())
            .map(str::to_owned)
            .collect();
        let workers: Vec<&str> = workers.iter().map(String::as_str).collect();
        snapshot.faults.push(row(
            "campaign",
            "campaign",
            "started",
            json!({ "campaign": "rolling-upgrade" }),
            -1,
        ));
        for (round, step) in UPGRADE_STEPS.iter().enumerate() {
            let sent = snapshot
                .events
                .iter()
                .find(|event| {
                    event.operation == "turn"
                        && event.phase == "sent"
                        && turn_key(&event.subject) == (round as u64, 0)
                })
                .expect("session 0's turn of the round")
                .recorded_at_us;
            let at = sent + 5;
            let (injected, recovered) = details(step, &workers);
            snapshot
                .faults
                .push(row(step, step, "intent", json!({}), at - 1));
            snapshot
                .faults
                .push(row(step, step, "injected", injected, at));
            snapshot
                .faults
                .push(row(step, step, "recovered", recovered, at + 1));
        }
        snapshot
            .faults
            .push(row("campaign", "campaign", "complete", json!({}), end));
        snapshot
    }

    fn position(snapshot: &WitnessSnapshot, kind: &str, phase: &str) -> usize {
        snapshot
            .faults
            .iter()
            .position(|row| row.kind == kind && row.phase == phase)
            .expect("a step row")
    }

    #[test]
    fn a_correct_rolling_upgrade_witnesses_every_upgrade_class() {
        let correct = verdict(&campaign(&smoke()));
        assert!(correct.passed(), "{:?}", correct.lines());
        assert_eq!(correct.classes.len(), CLASSES.len() + UPGRADE_CLASSES.len());
        for class in UPGRADE_CLASSES {
            assert!(correct.classes[class].witnessed > 0, "{class}");
        }

        // The campaign ends when the fence recovers, and the sessions stop
        // sending then: nothing need be sent after the last recovery.
        let mut ended = campaign(&smoke());
        let last = ended
            .events
            .iter()
            .map(|event| event.recorded_at_us)
            .max()
            .expect("events");
        let fence = position(&ended, "fence", "recovered");
        ended.faults[fence].recorded_at_us = last + 1;
        let ended = verdict(&ended);
        assert!(ended.passed(), "{:?}", ended.lines());
    }

    #[test]
    fn a_rollback_that_does_not_restore_n_is_a_violation() {
        let base = campaign(&smoke());

        // The rollback served some generation other than N's.
        let mut foreign = base.clone();
        let index = position(&foreign, "rollback", "injected");
        foreign.faults[index].detail["restored_generation"] = json!("g9");
        assert!(violated(&verdict(&foreign), "rollback"));

        // N+1 retired with work still pinned to it.
        let mut pinned = base.clone();
        let index = position(&pinned, "rollback", "recovered");
        pinned.faults[index].detail["pinned_unfinished"] = json!(2);
        assert!(violated(&verdict(&pinned), "rollback"));

        // The rollback ran after finalize.
        let mut late = base.clone();
        let finalize = late.faults[position(&late, "finalize", "recovered")].recorded_at_us;
        let rollback = position(&late, "rollback", "injected");
        late.faults[rollback].recorded_at_us = finalize + 1;
        let recovered = position(&late, "rollback", "recovered");
        late.faults[recovered].recorded_at_us = finalize + 2;
        assert!(violated(&verdict(&late), "roll"));
    }

    #[test]
    fn a_stale_writer_that_wrote_after_finalize_is_a_violation() {
        let base = campaign(&smoke());

        let mut wrote = base.clone();
        let index = position(&wrote, "fence", "injected");
        wrote.faults[index].detail["live_write"] =
            json!({ "status": 200, "body": "{\"marked\":true}" });
        assert!(violated(&verdict(&wrote), "fence"));

        let mut opened = base.clone();
        let index = position(&opened, "fence", "recovered");
        opened.faults[index].detail["fresh_open"]["exit"] = json!(0);
        assert!(violated(&verdict(&opened), "fence"));

        let mut unfinalized = base.clone();
        let index = position(&unfinalized, "finalize", "recovered");
        unfinalized.faults[index].detail["flip"] =
            json!({ "outcome": "already_finalized", "from": 2, "to": 2 });
        assert!(violated(&verdict(&unfinalized), "finalize"));

        let mut unretired = base.clone();
        let index = position(&unretired, "finalize", "injected");
        unretired.faults[index].detail["refused"] = json!(null);
        assert!(violated(&verdict(&unretired), "finalize"));
    }

    #[test]
    fn a_session_silent_after_a_step_or_a_lost_effect_is_a_violation() {
        let base = campaign(&smoke());

        // Session 0 answered nothing it sent after the roll.
        let mut silent = base.clone();
        let at = silent.faults[position(&silent, "roll", "injected")].recorded_at_us;
        let quiet: Vec<String> = silent
            .events
            .iter()
            .filter(|event| {
                event.operation == "turn"
                    && event.phase == "sent"
                    && event.recorded_at_us > at
                    && OperationId::parse(&event.subject).is_ok_and(|(id, _)| id.actor == 0)
            })
            .map(|event| event.subject.clone())
            .collect();
        assert!(!quiet.is_empty(), "session 0 sends after the roll");
        for event in &mut silent.events {
            if event.operation == "turn"
                && event.phase == "terminal"
                && quiet.contains(&event.subject)
            {
                event.detail = json!({ "error": "HTTP 503" });
            }
        }
        let silent = verdict(&silent);
        assert!(violated(&silent, "sessions-through-roll"));
        assert!(violated(&silent, "turns"));

        // A tool effect of an answered turn never committed across the roll.
        let mut lost = base.clone();
        let committed = lost
            .commits
            .iter()
            .position(|(key, _)| key.contains("/tool/"))
            .expect("a committed tool effect");
        lost.commits.remove(committed);
        assert!(violated(&verdict(&lost), "tools"));

        // A step the campaign skipped has no evidence.
        let mut skipped = base.clone();
        skipped.faults.retain(|row| row.kind != "fence");
        let skipped = verdict(&skipped);
        assert!(!skipped.passed());
        assert_eq!(skipped.classes["fence"].witnessed, 0);

        // The controller gave up on a step.
        let mut failed = base.clone();
        failed.faults.push(row(
            "roll",
            "roll",
            "failed",
            json!({ "reason": "drain watchdog" }),
            i64::MAX,
        ));
        assert!(violated(&verdict(&failed), "upgrade-campaign"));
    }
}
