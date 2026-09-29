//! Every obligation ends settled or stalled with a typed reason.
//!
//! ADR 0109: a row that owes the engine an effect carries the obligation
//! columns; at the end of a history each armed obligation is `delivered`, or
//! `stalled` with one of the typed stall reasons. A `due` row whose due time
//! is still ahead is scheduled: the relay backed off, or deferred work that
//! is not owed yet (ADR 0113 §2.5's `NotYet`), and the ledger holds it. A
//! `due` row whose time has come, or a `claimed` one, is work left
//! undelivered. An obligation its producer's own transaction arms (ADR
//! 0109 §3: an admitted ingress item, a terminal root's scope close) that is
//! not armed at all was dropped silently.
//!
//! An artifact cleanup is delivered only by the recovery pass's relay (ADR
//! 0113 §2.5). A history that never ran that pass leaves its cleanups `due`
//! for it; only a history that ran it to quiescence is judged on them.
//!
//! A claim is judged only once its claimant had its lapse: a crash world
//! whose deployment died inside a pass leaves that pass's claims held by
//! nobody until they lapse, so its history ends after the pass that follows
//! the last lapse (`check_crash_world`, FIG-4129). A row still claimed then
//! was retaken and not settled.

use std::collections::BTreeSet;

use super::{Fact, History, HistoryChecker, Violation};

pub(super) struct ObligationsSettled;

const INVARIANT: &str = "obligations-settled-or-stalled";

/// The ledger only the recovery pass's relay delivers.
pub(super) const RELAY_ONLY: &str = "artifact_cleanup_obligations";

/// `StallReason` labels (lash-core-store).
const STALL_REASONS: [&str; 3] = ["attempts_exhausted", "refused", "undecodable"];

impl HistoryChecker for ObligationsSettled {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        history
            .stores
            .iter()
            .flat_map(|store| &store.obligations)
            .filter(|row| row.state.is_some())
            .count()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let mut violations = Vec::new();
        for store in &history.stores {
            let terminal_roots = store
                .roots
                .iter()
                .filter(|root| root.terminal_kind.is_some())
                .map(|root| format!("{}/{}", root.session, root.root))
                .collect::<BTreeSet<_>>();
            for row in &store.obligations {
                let problem = match row.state.as_deref() {
                    Some("delivered") => None,
                    Some("stalled") => match row.stall_reason.as_deref() {
                        Some(reason) if STALL_REASONS.contains(&reason) => None,
                        reason => Some(format!("stalled without a typed reason ({reason:?})")),
                    },
                    Some("due") if !history.relay_ran && row.table == RELAY_ONLY => None,
                    Some("due")
                        if row
                            .due_at_ms
                            .zip(history.now_ms)
                            .is_some_and(|(due, now)| due > now) =>
                    {
                        None
                    }
                    Some(state @ ("due" | "claimed")) => Some(format!(
                        "left {state} at {}: neither delivered nor stalled",
                        history
                            .now_ms
                            .map_or_else(|| "the end".to_owned(), |now| format!("{now} ms"))
                    )),
                    Some(state) => Some(format!("in unknown state `{state}`")),
                    None => {
                        let owed = match row.table.as_str() {
                            "pending_turn_inputs" | "queued_work_batches" => {
                                Some("its admission armed no ingress obligation")
                            }
                            "session_roots" if terminal_roots.contains(&row.key) => {
                                Some("its terminal transaction armed no scope close")
                            }
                            _ => None,
                        };
                        owed.map(str::to_owned)
                    }
                };
                if let Some(problem) = problem {
                    // An ingress row's input says what became of it.
                    let input = row.id.as_deref().and_then(|id| {
                        let item = id.strip_prefix("ingress:")?;
                        store
                            .inputs
                            .iter()
                            .find(|input| input.table == row.table && input.id == item)
                    });
                    let about = input.map_or_else(String::new, |input| {
                        format!(
                            " (input {}, state {})",
                            input.id,
                            input.state.as_deref().unwrap_or("queued")
                        )
                    });
                    let mut violation = Violation::new(
                        INVARIANT,
                        format!(
                            "{} obligation on {} {}{about} {problem}",
                            store.label, row.table, row.key
                        ),
                    )
                    .row(row.render());
                    if let Some(input) = input {
                        violation = violation
                            .session(input.session.clone())
                            .records(history.records.iter().filter_map(|record| {
                                matches!(
                                    &record.fact,
                                    Fact::Boundary { input: Some(named), .. } if *named == input.id
                                )
                                .then_some(record.at)
                            }))
                            .row(input.render());
                    }
                    violations.push(violation);
                }
            }
        }
        violations
    }
}
