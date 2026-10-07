//! Every admitted input settles exactly once.
//!
//! ADR 0101 §7: an admitted input is either executed
//! by exactly one run, bound to it in `session_run_inputs`, and settled
//! when that run ends (completed, or cancelled with it); or it is cancelled
//! unexecuted. At the end of a history no input is still open, no completed
//! input lacks its one run, and no input is settled while the run bound to
//! it still runs. A queued-work batch a run admitted is
//! settled with it, so none is left behind an ended run or unadmitted.

use std::collections::BTreeMap;

use super::{Fact, History, HistoryChecker, Violation};

pub(super) struct InputSettlement;

const INVARIANT: &str = "input-settles-exactly-once";

impl HistoryChecker for InputSettlement {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        history.stores.iter().map(|store| store.inputs.len()).sum()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let mut violations = Vec::new();
        for store in &history.stores {
            let runs = store
                .runs
                .iter()
                .map(|run| ((run.session.as_str(), run.run.as_str()), run))
                .collect::<BTreeMap<_, _>>();
            let bindings = store
                .run_inputs
                .iter()
                .map(|(session, input, run)| ((session.as_str(), input.as_str()), run.as_str()))
                .collect::<BTreeMap<_, _>>();
            for input in &store.inputs {
                let bound = bindings
                    .get(&(input.session.as_str(), input.id.as_str()))
                    .copied();
                let run_of = |run: &str| runs.get(&(input.session.as_str(), run)).copied();
                let problem = match (input.table.as_str(), input.state.as_deref()) {
                    ("pending_turn_inputs", Some("completed")) => match bound {
                        None => Some("completed, but no run is bound to it".to_owned()),
                        Some(run) => match run_of(run) {
                            Some(row) if row.terminal_kind.is_some() => None,
                            Some(_) => {
                                Some(format!("completed while its run {run} is not terminal"))
                            }
                            None => Some(format!("completed by run {run}, which has no row")),
                        },
                    },
                    ("pending_turn_inputs", Some("cancelled")) => {
                        bound.and_then(|run| match run_of(run) {
                            Some(row) if row.terminal_kind.is_some() => None,
                            Some(_) => Some(format!(
                                "cancelled while run {run}, which executes it, still runs"
                            )),
                            None => Some(format!(
                                "cancelled, and bound to run {run}, which has no row"
                            )),
                        })
                    }
                    ("pending_turn_inputs", state) => Some(format!(
                        "left {} at the end of the history",
                        state.unwrap_or("stateless")
                    )),
                    // A settled batch stays as its tombstone (ADR 0101 §8).
                    ("queued_work_batches", Some(_)) => None,
                    _ => Some(match &input.admitted_run {
                        None => "queued batch left unadmitted at the end of the history".to_owned(),
                        Some(run) => format!(
                            "queued batch left behind its run {run} ({})",
                            run_of(run)
                                .and_then(|row| row.terminal_kind.as_deref())
                                .unwrap_or("not terminal")
                        ),
                    }),
                };
                let twice = match (&input.admitted_run, bound) {
                    (Some(admitted), Some(bound)) if admitted != bound => Some(format!(
                        "admitted by run {admitted} but bound to run {bound}"
                    )),
                    _ => None,
                };
                for problem in problem.into_iter().chain(twice) {
                    let mut violation = Violation::new(
                        INVARIANT,
                        format!(
                            "{}: {} {}/{} {problem}",
                            store.label, input.table, input.session, input.id
                        ),
                    )
                    .session(input.session.clone())
                    .records(history.records.iter().filter_map(|record| {
                        matches!(
                            &record.fact,
                            Fact::Boundary { input: Some(named), .. } if *named == input.id
                        )
                        .then_some(record.at)
                    }))
                    .row(input.render());
                    if let Some(run) = bound.and_then(run_of) {
                        violation = violation.row(run.render());
                    }
                    violations.push(violation);
                }
            }
        }
        violations
    }
}
