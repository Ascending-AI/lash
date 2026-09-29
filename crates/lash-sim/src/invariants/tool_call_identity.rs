//! Tool-call identities are unique per logical call and stable across replay.
//!
//! Keyed on today's identity, [`CallIdentity`](super::CallIdentity): the
//! call id the tool sees. Every run of one logical call — crash replays and
//! reported-failure retries alike — sees one id, and two logical calls never
//! share one: not among the tool's runs in a session, and not among the tool
//! calls a session's committed transcript holds.
//!
//! FIG-4080 switches the identity to `lash_sansio::ToolCallId`; see the
//! extension point on [`CallIdentity`](super::CallIdentity).

use std::collections::{BTreeMap, BTreeSet};

use super::{CallIdentity, Fact, History, HistoryChecker, Violation};

pub(super) struct ToolCallIdentity;

/// One logical call: session, scope, engine address.
type LogicalKey<'a> = (&'a str, &'a str, &'a str);

/// One id at one attempt: session, scope, id, attempt number.
type IdentityKey<'a> = (&'a str, &'a str, &'a CallIdentity, u32);

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
        // Stable: one logical call, one id, on every run and registration.
        let mut by_logical: BTreeMap<LogicalKey<'_>, Vec<(usize, &CallIdentity)>> = BTreeMap::new();
        // Unique: one id, one logical call, per attempt.
        let mut by_identity: BTreeMap<IdentityKey<'_>, Vec<(usize, &str)>> = BTreeMap::new();
        for record in &history.records {
            let (call, attempt) = match &record.fact {
                Fact::ToolExecuted { call, attempt, .. } => (call, Some(*attempt)),
                Fact::CompletionRegistered { call, .. } => (call, None),
                _ => continue,
            };
            if !call.logical.is_empty() {
                by_logical
                    .entry((&call.session, &call.scope, &call.logical))
                    .or_default()
                    .push((record.at, &call.identity));
            }
            if let Some(attempt) = attempt
                && !call.identity.0.is_empty()
                && !call.logical.is_empty()
            {
                by_identity
                    .entry((&call.session, &call.scope, &call.identity, attempt))
                    .or_default()
                    .push((record.at, &call.logical));
            }
        }
        for ((session, scope, logical), seen) in by_logical {
            let identities = seen.iter().map(|(_, id)| *id).collect::<BTreeSet<_>>();
            if identities.len() > 1 {
                violations.push(
                    Violation::new(
                        INVARIANT,
                        format!(
                            "logical call {logical} in scope {scope} ran under {} ids: {}",
                            identities.len(),
                            identities
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                    .records(seen.iter().map(|(at, _)| *at))
                    .session(session),
                );
            }
        }
        for ((session, scope, identity, attempt), seen) in by_identity {
            let logicals = seen
                .iter()
                .map(|(_, logical)| *logical)
                .collect::<BTreeSet<_>>();
            if logicals.len() > 1 {
                violations.push(
                    Violation::new(
                        INVARIANT,
                        format!(
                            "id {identity} named {} logical calls in scope {scope} at attempt {attempt}: {}",
                            logicals.len(),
                            logicals.into_iter().collect::<Vec<_>>().join(", ")
                        ),
                    )
                    .records(seen.iter().map(|(at, _)| *at))
                    .session(session),
                );
            }
        }
        for store in &history.stores {
            for transcript in &store.transcripts {
                let mut positions: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
                for call in &transcript.calls {
                    positions
                        .entry(&call.call_id)
                        .or_default()
                        .push((call.message, call.index));
                }
                for (call_id, at) in positions {
                    if at.len() > 1 {
                        violations.push(
                            Violation::new(
                                INVARIANT,
                                format!(
                                    "{}: session {} committed {} tool calls under one id {call_id}",
                                    store.label,
                                    transcript.session,
                                    at.len()
                                ),
                            )
                            .session(transcript.session.clone())
                            .row(format!(
                                "committed tool calls {call_id} at (assistant message, part) {at:?}"
                            )),
                        );
                    }
                }
            }
        }
        violations
    }
}
