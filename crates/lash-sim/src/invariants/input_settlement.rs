//! Every admitted input settles exactly once.
//!
//! ADR 0101 and its FIG-3927 amendment: an admitted input is either driven
//! by exactly one root, bound to it in `session_root_inputs`, and settled
//! when that root ends (completed, or cancelled with it); or it is cancelled
//! undriven. At the end of a history no input is still open, no completed
//! input lacks its one root, and no input is settled while the root bound to
//! it still runs. An input left open because its delivery stalled with a
//! typed reason (its ingress obligation is `stalled`: an operator's to act
//! on, ADR 0109 §1.5) is surfaced, not lost; the settled-or-stalled checker
//! judges that stall. A queued-work batch a root admitted is
//! settled with it, so none is left behind an ended root or unadmitted.

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
            let roots = store
                .roots
                .iter()
                .map(|root| ((root.session.as_str(), root.root.as_str()), root))
                .collect::<BTreeMap<_, _>>();
            let bindings = store
                .root_inputs
                .iter()
                .map(|(session, input, root)| ((session.as_str(), input.as_str()), root.as_str()))
                .collect::<BTreeMap<_, _>>();
            for input in &store.inputs {
                let bound = bindings
                    .get(&(input.session.as_str(), input.id.as_str()))
                    .copied();
                let root_of = |root: &str| roots.get(&(input.session.as_str(), root)).copied();
                let problem = match (input.table.as_str(), input.state.as_deref()) {
                    ("pending_turn_inputs", Some("completed")) => match bound {
                        None => Some("completed, but no root is bound to it".to_owned()),
                        Some(root) => match root_of(root) {
                            Some(row) if row.terminal_kind.is_some() => None,
                            Some(_) => {
                                Some(format!("completed while its root {root} is not terminal"))
                            }
                            None => Some(format!("completed by root {root}, which has no row")),
                        },
                    },
                    ("pending_turn_inputs", Some("cancelled")) => {
                        bound.and_then(|root| match root_of(root) {
                            Some(row) if row.terminal_kind.is_some() => None,
                            Some(_) => Some(format!(
                                "cancelled while root {root}, which drives it, still runs"
                            )),
                            None => Some(format!(
                                "cancelled, and bound to root {root}, which has no row"
                            )),
                        })
                    }
                    ("pending_turn_inputs", _)
                        if input.obligation_state.as_deref() == Some("stalled") =>
                    {
                        None
                    }
                    ("pending_turn_inputs", state) => Some(format!(
                        "left {} at the end of the history",
                        state.unwrap_or("stateless")
                    )),
                    _ => Some(match &input.admitted_root {
                        None => "queued batch left unadmitted at the end of the history".to_owned(),
                        Some(root) => format!(
                            "queued batch left behind its root {root} ({})",
                            root_of(root)
                                .and_then(|row| row.terminal_kind.as_deref())
                                .unwrap_or("not terminal")
                        ),
                    }),
                };
                let twice = match (&input.admitted_root, bound) {
                    (Some(admitted), Some(bound)) if admitted != bound => Some(format!(
                        "admitted by root {admitted} but bound to root {bound}"
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
                    if let Some(root) = bound.and_then(root_of) {
                        violation = violation.row(root.render());
                    }
                    violations.push(violation);
                }
            }
        }
        violations
    }
}
