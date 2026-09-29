//! Every completion is consumed only by the call that owns it.
//!
//! A deferring call registers its completion key; the host resolves the key;
//! the resolution reaches the transcript as a tool result. Two calls must
//! never register one key (then an unrelated call can take the other's
//! resolution, FIG-4073), and the committed result that carries a resolution
//! must be the owning call's.

use std::collections::{BTreeMap, BTreeSet};

use super::{CallRef, Fact, History, HistoryChecker, Violation};

pub(super) struct CompletionOwnership;

const INVARIANT: &str = "completion-ownership";

impl HistoryChecker for CompletionOwnership {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        history
            .records
            .iter()
            .filter(|record| {
                matches!(
                    record.fact,
                    Fact::CompletionRegistered { .. } | Fact::CompletionResolved { .. }
                )
            })
            .count()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let mut owners: BTreeMap<&str, Vec<(usize, &CallRef)>> = BTreeMap::new();
        for record in &history.records {
            if let Fact::CompletionRegistered { key, call } = &record.fact {
                owners.entry(key).or_default().push((record.at, call));
            }
        }
        let mut violations = Vec::new();
        for (key, registered) in &owners {
            let calls = registered
                .iter()
                .map(|(_, call)| (&call.session, &call.scope, &call.identity))
                .collect::<BTreeSet<_>>();
            if calls.len() > 1 {
                violations.push(
                    Violation::new(
                        INVARIANT,
                        format!(
                            "{} distinct calls registered one completion key {key}: {}",
                            calls.len(),
                            registered
                                .iter()
                                .map(|(_, call)| format!("{} in {}", call.identity, call.scope))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                    .records(registered.iter().map(|(at, _)| *at))
                    .session(registered[0].1.session.clone()),
                );
            }
        }
        for record in &history.records {
            let Fact::CompletionResolved {
                key,
                session,
                result_digest,
            } = &record.fact
            else {
                continue;
            };
            let Some(registered) = owners.get(key.as_str()) else {
                violations.push(
                    Violation::new(
                        INVARIANT,
                        format!("completion {key} was resolved but no call registered it"),
                    )
                    .records([record.at])
                    .session(session.clone()),
                );
                continue;
            };
            let owner = registered[0].1;
            let consumers = history
                .stores
                .iter()
                .filter_map(|store| store.transcript(session))
                .flat_map(|transcript| &transcript.results)
                .filter(|result| &result.digest == result_digest)
                .collect::<Vec<_>>();
            let evidence = registered
                .iter()
                .map(|(at, _)| *at)
                .chain([record.at])
                .collect::<Vec<_>>();
            for consumer in &consumers {
                if consumer.call_id != owner.identity.0 {
                    violations.push(
                        Violation::new(
                            INVARIANT,
                            format!(
                                "completion {key}, owned by call {}, was consumed by call {}",
                                owner.identity, consumer.call_id
                            ),
                        )
                        .records(evidence.clone())
                        .row(format!(
                            "tool result of call {} ({}) carries the resolution {result_digest}",
                            consumer.call_id, consumer.tool
                        )),
                    );
                }
            }
            if consumers.len() > 1 {
                violations.push(
                    Violation::new(
                        INVARIANT,
                        format!(
                            "completion {key} was consumed {} times: by calls {}",
                            consumers.len(),
                            consumers
                                .iter()
                                .map(|consumer| consumer.call_id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                    .records(evidence),
                );
            }
        }
        violations
    }
}
