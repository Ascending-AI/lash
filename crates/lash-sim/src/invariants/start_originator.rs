//! An existing start is answered only to its originator.
//!
//! ADR 0107 (FIG-4111): a host start key is global, so two originators can
//! present the same key. A start under a retained key returns the retained
//! process (`Existing`) only to the start that made it; any other start is a
//! typed conflict. So every `Existing` answer names a process whose originator
//! is the one the answered request carried.

use super::{Fact, History, HistoryChecker, Violation};

pub(super) struct StartOriginator;

const INVARIANT: &str = "an-existing-start-is-answered-only-to-its-originator";

impl HistoryChecker for StartOriginator {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        history
            .records
            .iter()
            .filter(|record| matches!(record.fact, Fact::ProcessStartAnswered { .. }))
            .count()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        history
            .records
            .iter()
            .filter_map(|record| match &record.fact {
                Fact::ProcessStartAnswered {
                    process,
                    disposition,
                    requested_originator,
                    answered_originator,
                } if disposition == "existing" && requested_originator != answered_originator => {
                    Some(
                        Violation::new(
                            INVARIANT,
                            format!(
                                "a start from {requested_originator} was answered the existing \
                                 process {process}, which {answered_originator} started"
                            ),
                        )
                        .records([record.at]),
                    )
                }
                _ => None,
            })
            .collect()
    }
}
