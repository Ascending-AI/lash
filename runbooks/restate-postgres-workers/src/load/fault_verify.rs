//! The fault classes of a load run under the fault controller (FIG-4169).
//! The controller's ledger and the load events share the witness database's
//! clock, so each fault is placed against the operations it hit: they must
//! have been in flight, reached durable answers, and service must have gone
//! on after the fault.

use super::verify::{FaultRow, WitnessSnapshot, sent_request};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One operation's witnessed life on the witness clock: when the driver
/// sent it, and when and how it read back its terminal.
struct Timeline<'a> {
    operation: &'a str,
    subject: &'a str,
    /// The Restate workflow key the operation ran under.
    workflow_key: Option<String>,
    sent_at: i64,
    terminal: Option<(i64, &'a Value)>,
}

impl Timeline<'_> {
    fn answered(&self) -> bool {
        self.terminal
            .is_some_and(|(_, detail)| detail.get("response").is_some())
    }

    fn in_flight_at(&self, at: i64) -> bool {
        self.sent_at < at && self.terminal.is_none_or(|(ended, _)| ended > at)
    }

    fn response(&self) -> Option<&Value> {
        self.terminal.and_then(|(_, detail)| detail.get("response"))
    }
}

fn timelines(snapshot: &WitnessSnapshot) -> Vec<Timeline<'_>> {
    let mut index: BTreeMap<(&str, &str), Timeline<'_>> = BTreeMap::new();
    for event in &snapshot.events {
        if event.operation == "attachment" {
            continue;
        }
        let key = (event.operation.as_str(), event.subject.as_str());
        match event.phase.as_str() {
            "sent" => {
                index.entry(key).or_insert(Timeline {
                    operation: key.0,
                    subject: key.1,
                    workflow_key: sent_request(&event.detail).map(|request| request.workflow_key()),
                    sent_at: event.recorded_at_us,
                    terminal: None,
                });
            }
            "terminal" => {
                if let Some(timeline) = index.get_mut(&key) {
                    timeline.terminal = Some((event.recorded_at_us, &event.detail));
                }
            }
            _ => {}
        }
    }
    index.into_values().collect()
}

/// The fault classes of a campaign run. Each fault the controller injected
/// must have hit load operations in flight, every one of them must have
/// reached a durable answer, service must have progressed after the fault
/// (turns, queued inputs and cron emissions), and the controller's own
/// recovery evidence must hold. A fault kind with no injected fault has no
/// evidence, so a campaign that skipped one fails its class.
pub(super) fn verify_faults(
    snapshot: &WitnessSnapshot,
    note: &mut impl FnMut(&'static str, Result<(), String>),
) {
    let timelines = timelines(snapshot);
    let rows = |fault_id: &str, phase: &str| -> Option<&FaultRow> {
        snapshot
            .faults
            .iter()
            .find(|row| row.fault_id == fault_id && row.phase == phase)
    };
    let campaign: Vec<&FaultRow> = snapshot
        .faults
        .iter()
        .filter(|row| row.kind == "campaign")
        .collect();
    note(
        "fault-campaign",
        match (
            campaign.iter().find(|row| row.phase == "started"),
            campaign.iter().find(|row| row.phase == "complete"),
            snapshot.faults.iter().find(|row| row.phase == "failed"),
        ) {
            (_, _, Some(failed)) => Err(format!(
                "{} `{}` failed: {}",
                failed.kind, failed.fault_id, failed.detail
            )),
            (Some(_), Some(_), None) => Ok(()),
            (started, complete, None) => Err(format!(
                "the campaign started={} complete={}",
                started.is_some(),
                complete.is_some()
            )),
        },
    );
    let mut faults: Vec<(&str, &str)> = Vec::new();
    for row in &snapshot.faults {
        if row.kind != "campaign" && !faults.contains(&(row.kind.as_str(), row.fault_id.as_str())) {
            faults.push((row.kind.as_str(), row.fault_id.as_str()));
        }
    }
    for (kind, fault_id) in faults {
        let class = match kind {
            "worker-kill" => "worker-kill",
            "restate-restart" => "restate-restart",
            "rolling-deploy" => "rolling-deploy",
            _ => {
                note(
                    "fault-campaign",
                    Err(format!("fault `{fault_id}` has unknown kind `{kind}`")),
                );
                continue;
            }
        };
        let (Some(injected), Some(recovered)) =
            (rows(fault_id, "injected"), rows(fault_id, "recovered"))
        else {
            note(
                class,
                Err(format!(
                    "{kind} `{fault_id}` was not both injected and recovered"
                )),
            );
            continue;
        };
        let at = injected.recorded_at_us;
        if recovered.recorded_at_us < at {
            note(
                class,
                Err(format!(
                    "{kind} `{fault_id}` recovered before it was injected"
                )),
            );
            continue;
        }
        let hit: Vec<&Timeline<'_>> = timelines
            .iter()
            .filter(|timeline| timeline.in_flight_at(at))
            .collect();
        note(
            class,
            if hit.is_empty() {
                Err(format!(
                    "{kind} `{fault_id}` missed active work: no load operation was in flight"
                ))
            } else {
                Ok(())
            },
        );
        for timeline in &hit {
            if !timeline.answered() {
                note(
                    class,
                    Err(format!(
                        "{} `{}` in flight at {kind} `{fault_id}` never reached a durable answer: {:?}",
                        timeline.operation,
                        timeline.subject,
                        timeline.terminal.map(|(_, detail)| detail)
                    )),
                );
            }
        }
        // The busy work the controller saw on its target, sampled just before
        // the injection, must be witnessed load work sent before the fault
        // that then answered. The witness's own in-flight set above is the
        // independent proof that the fault hit work.
        for key in injected.detail["active"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let witnessed = timelines.iter().any(|timeline| {
                timeline.workflow_key.as_deref() == Some(key)
                    && timeline.sent_at < at
                    && timeline.answered()
            });
            if !witnessed {
                note(
                    class,
                    Err(format!(
                        "{kind} `{fault_id}` named busy work `{key}` that the witness did not see sent before it and answered"
                    )),
                );
            }
        }
        let after = |operation: &str, accept: &dyn Fn(&Value) -> bool| {
            timelines.iter().any(|timeline| {
                timeline.operation == operation
                    && timeline.sent_at > at
                    && timeline.response().is_some_and(accept)
            })
        };
        let turn_answered = after("turn", &|response| {
            response["outcome"]["status"] == "answered"
        });
        let queued_answered = after("turn", &|response| {
            response["queued"].as_array().is_some_and(|queued| {
                queued
                    .iter()
                    .any(|input| input["outcome"]["status"] == "answered")
            })
        });
        let cron_ticked = after("cron-tick", &|response| {
            response["started_process_ids"]
                .as_array()
                .is_some_and(|started| !started.is_empty())
        });
        if !(turn_answered && queued_answered && cron_ticked) {
            note(
                class,
                Err(format!(
                    "service did not progress after {kind} `{fault_id}`: turn={turn_answered} queued={queued_answered} cron={cron_ticked}"
                )),
            );
        }
        let recovery = &recovered.detail;
        let held = match kind {
            "worker-kill" => {
                recovery["same_pod"] == true
                    && recovery["restarts_after"].as_u64() > recovery["restarts_before"].as_u64()
                    && injected.detail["signal"] == "KILL"
            }
            "restate-restart" => {
                recovery["same_node_id"] == true
                    && recovery["leaders"] == recovery["partitions"]
                    && recovery["generation_after"] != injected.detail["generation_before"]
                    && injected.detail["advancing_partitions"]
                        .as_array()
                        .is_some_and(|partitions| !partitions.is_empty())
            }
            _ => {
                let new_workers: BTreeSet<&str> = injected.detail["new_workers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let moved = after("turn", &|response| {
                    response["worker_id"]
                        .as_str()
                        .is_some_and(|worker| new_workers.contains(worker))
                });
                recovery["drained"] == true
                    && recovery["pinned_unfinished"] == 0
                    && recovery["stalled_total"] == 0
                    && injected.detail["old_generation"] != injected.detail["new_generation"]
                    && moved
            }
        };
        note(
            class,
            if held {
                Ok(())
            } else {
                Err(format!(
                    "{kind} `{fault_id}` recovery evidence does not hold: injected {} recovered {recovery}",
                    injected.detail
                ))
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::verify::tests::{ideal, smoke, verdict, violated};
    use super::super::verify::{CLASSES, FAULT_CLASSES, FaultRow, WitnessSnapshot, sent_request};
    use crate::load::LoadContext;
    use serde_json::{Value, json};

    fn fault(fault_id: &str, kind: &str, phase: &str, detail: Value, at: i64) -> FaultRow {
        FaultRow {
            fault_id: fault_id.to_owned(),
            kind: kind.to_owned(),
            phase: phase.to_owned(),
            target: format!("{kind}-target"),
            detail,
            recorded_at_us: at,
        }
    }

    /// A correct smoke run under a campaign: each of the three faults lands
    /// while one answered turn is in flight, names it as the busy work, and
    /// recovers with the controller's evidence.
    fn campaign(load: &LoadContext) -> WitnessSnapshot {
        let mut snapshot = ideal(load, lash_perf::workload::SMOKE_TURNS_PER_SESSION);
        for (index, event) in snapshot.events.iter_mut().enumerate() {
            event.recorded_at_us = index as i64 * 10;
        }
        let busy: Vec<(i64, String)> = snapshot
            .events
            .windows(2)
            .filter(|pair| {
                pair[0].operation == "turn"
                    && pair[0].phase == "sent"
                    && pair[1].phase == "terminal"
                    && pair[1].detail["response"]["outcome"]["status"] == "answered"
            })
            .take(3)
            .map(|pair| {
                let key = sent_request(&pair[0].detail)
                    .expect("a sent request")
                    .workflow_key();
                (pair[0].recorded_at_us + 5, key)
            })
            .collect();
        assert_eq!(busy.len(), 3, "the ideal run has three answered turns");
        let end = snapshot.events.len() as i64 * 10;
        snapshot
            .faults
            .push(fault("campaign", "campaign", "started", json!({}), -1));
        let details = [
            (
                "worker-kill",
                json!({ "signal": "KILL" }),
                json!({ "same_pod": true, "restarts_before": 0, "restarts_after": 1 }),
            ),
            (
                "restate-restart",
                json!({ "generation_before": "N2:2", "advancing_partitions": [3] }),
                json!({ "same_node_id": true, "generation_after": "N2:3", "leaders": 24, "partitions": 24 }),
            ),
            (
                "rolling-deploy",
                json!({ "old_generation": "g0", "new_generation": "g1", "new_workers": ["worker-a"] }),
                json!({ "drained": true, "pinned_unfinished": 0, "stalled_total": 0 }),
            ),
        ];
        for ((kind, mut injected, recovered), (at, key)) in details.into_iter().zip(busy) {
            injected["active"] = json!([key]);
            snapshot
                .faults
                .push(fault(kind, kind, "intent", json!({}), at - 1));
            snapshot
                .faults
                .push(fault(kind, kind, "injected", injected, at));
            snapshot
                .faults
                .push(fault(kind, kind, "recovered", recovered, at + 2));
        }
        snapshot
            .faults
            .push(fault("campaign", "campaign", "complete", json!({}), end));
        snapshot
    }

    #[test]
    fn a_correct_campaign_witnesses_every_fault_class() {
        let verdict = verdict(&campaign(&smoke()));
        assert!(verdict.passed(), "{:?}", verdict.lines());
        assert_eq!(verdict.classes.len(), CLASSES.len() + FAULT_CLASSES.len());
        for class in FAULT_CLASSES {
            assert!(verdict.classes[class].witnessed > 0, "{class}");
        }
    }

    #[test]
    fn a_fault_that_misses_work_or_loses_an_answer_is_a_violation() {
        let base = campaign(&smoke());
        let row = |snapshot: &WitnessSnapshot, kind: &str, phase: &str| -> usize {
            snapshot
                .faults
                .iter()
                .position(|row| row.kind == kind && row.phase == phase)
                .expect("a fault row")
        };

        // Injected before anything was sent: no work in flight, and the
        // named busy work was sent after the fault.
        let mut idle = base.clone();
        let index = row(&idle, "worker-kill", "injected");
        idle.faults[index].recorded_at_us = -5;
        let recovered = row(&idle, "worker-kill", "recovered");
        idle.faults[recovered].recorded_at_us = -4;
        let idle = verdict(&idle);
        assert!(violated(&idle, "worker-kill"));
        assert!(
            idle.classes["worker-kill"]
                .violations
                .iter()
                .any(|violation| violation.contains("missed active work"))
        );

        // The turn the Restate restart hit read back only a failure.
        let mut lost = base.clone();
        let at = lost.faults[row(&lost, "restate-restart", "injected")].recorded_at_us;
        let terminal = lost
            .events
            .iter()
            .position(|event| {
                event.operation == "turn" && event.phase == "terminal" && event.recorded_at_us > at
            })
            .expect("the hit turn's terminal");
        lost.events[terminal].detail = json!({ "error": "HTTP 503" });
        let lost = verdict(&lost);
        assert!(violated(&lost, "restate-restart"));
        assert!(violated(&lost, "turns"));

        // A rolling deploy that retired with pinned work left.
        let mut pinned = base.clone();
        let index = row(&pinned, "rolling-deploy", "recovered");
        pinned.faults[index].detail["pinned_unfinished"] = json!(1);
        assert!(violated(&verdict(&pinned), "rolling-deploy"));

        // Admission never moved to the replacement build.
        let mut unmoved = base.clone();
        let index = row(&unmoved, "rolling-deploy", "injected");
        unmoved.faults[index].detail["new_workers"] = json!(["worker-next"]);
        assert!(violated(&verdict(&unmoved), "rolling-deploy"));

        // A restart of a node whose partitions were not processing work.
        let mut quiet = base.clone();
        let index = row(&quiet, "restate-restart", "injected");
        quiet.faults[index].detail["advancing_partitions"] = json!([]);
        assert!(violated(&verdict(&quiet), "restate-restart"));

        // A worker that came back as a different pod.
        let mut replaced = base.clone();
        let index = row(&replaced, "worker-kill", "recovered");
        replaced.faults[index].detail["same_pod"] = json!(false);
        assert!(violated(&verdict(&replaced), "worker-kill"));

        // No cron emission after the fault.
        let mut stalled = base.clone();
        stalled
            .events
            .retain(|event| event.operation != "cron-tick");
        assert!(violated(&verdict(&stalled), "worker-kill"));

        // The controller gave up on a fault.
        let mut failed = base.clone();
        failed.faults.push(fault(
            "rolling-deploy",
            "rolling-deploy",
            "failed",
            json!({ "reason": "drain watchdog" }),
            i64::MAX,
        ));
        assert!(violated(&verdict(&failed), "fault-campaign"));

        // A campaign that never restarted Restate has no evidence there.
        let mut skipped = base.clone();
        skipped.faults.retain(|row| row.kind != "restate-restart");
        let skipped = verdict(&skipped);
        assert!(!skipped.passed());
        assert_eq!(skipped.classes["restate-restart"].witnessed, 0);
    }
}
