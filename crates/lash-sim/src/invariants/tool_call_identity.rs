//! Tool-call identities are stable across replay and unique across the
//! deployment.
//!
//! Keyed on [`CallIdentity`](super::CallIdentity): the `ToolCallId` the tool
//! sees (ADR 0117). Every run of one call — crash replays and
//! reported-failure retries alike — sees one id, so the attempts run under an
//! id count up from the first: an id whose runs begin past attempt 1 is a
//! call whose earlier attempts ran under another id. Two calls never share
//! one: no id is committed at two transcript positions, in one session or
//! across every session of every store.

use std::collections::{BTreeMap, BTreeSet};

use super::{CallIdentity, Fact, History, HistoryChecker, Violation};

pub(super) struct ToolCallIdentity;

/// One call's runs: session, scope, id.
type RunKey<'a> = (&'a str, &'a str, &'a CallIdentity);

const INVARIANT: &str = "tool-call-identity";

impl HistoryChecker for ToolCallIdentity {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        let runs = history
            .records
            .iter()
            .filter(|record| matches!(record.fact, Fact::ToolExecuted { .. }))
            .count();
        let committed = history
            .stores
            .iter()
            .flat_map(|store| &store.transcripts)
            .map(|transcript| transcript.calls.len())
            .sum::<usize>();
        runs + committed
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let mut violations = Vec::new();
        // Stable: the attempts of one id count up from the first.
        let mut attempts: BTreeMap<RunKey<'_>, Vec<(usize, u32)>> = BTreeMap::new();
        for record in &history.records {
            let Fact::ToolExecuted { call, attempt, .. } = &record.fact else {
                continue;
            };
            attempts
                .entry((&call.session, &call.scope, &call.identity))
                .or_default()
                .push((record.at, *attempt));
        }
        for ((session, scope, identity), seen) in attempts {
            let numbers = seen
                .iter()
                .map(|(_, attempt)| *attempt)
                .collect::<BTreeSet<_>>();
            let expected = (1..=numbers.last().copied().unwrap_or(0)).collect::<BTreeSet<_>>();
            if numbers != expected {
                violations.push(
                    Violation::new(
                        INVARIANT,
                        format!(
                            "id {identity} in scope {scope} ran attempts {numbers:?}: an earlier \
                             attempt of the call ran under another id"
                        ),
                    )
                    .records(seen.iter().map(|(at, _)| *at))
                    .session(session),
                );
            }
        }
        // Unique: one id, one committed position, deployment-wide.
        let mut positions: BTreeMap<&str, Vec<(&str, &str, usize, usize)>> = BTreeMap::new();
        for store in &history.stores {
            for transcript in &store.transcripts {
                for call in &transcript.calls {
                    positions.entry(&call.call_id).or_default().push((
                        &store.label,
                        &transcript.session,
                        call.message,
                        call.index,
                    ));
                }
            }
        }
        for (call_id, at) in positions {
            let distinct = at
                .iter()
                .map(|(_, session, message, index)| (*session, *message, *index))
                .collect::<BTreeSet<_>>();
            if distinct.len() > 1 {
                violations.push(
                    Violation::new(
                        INVARIANT,
                        format!(
                            "{} committed tool calls share one id {call_id}",
                            distinct.len()
                        ),
                    )
                    .session(at[0].1.to_owned())
                    .row(format!(
                        "committed tool calls {call_id} at (store, session, assistant message, \
                         part) {at:?}"
                    )),
                );
            }
        }
        violations
    }
}
