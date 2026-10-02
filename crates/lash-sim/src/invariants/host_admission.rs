//! Host acceptance and the durable input/transcript are independent witnesses.

use std::collections::{BTreeMap, BTreeSet};

use super::{Fact, History, HistoryChecker, HostOp, HostOutcome, Violation};

pub(super) struct HostAdmission;
const INVARIANT: &str = "host-admission";

pub(super) fn deleted(history: &History) -> BTreeSet<&str> {
    history
        .records
        .iter()
        .filter_map(|record| match &record.fact {
            Fact::HostOp {
                op: HostOp::Delete,
                session,
                outcome: HostOutcome::Known | HostOutcome::Maybe,
                ..
            } => Some(session.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn downgraded(history: &History) -> usize {
    let deleted = deleted(history);
    history.records.iter().filter(|record| matches!(&record.fact,
        Fact::HostOp { op: HostOp::Send | HostOp::SendBatch, session, .. } if deleted.contains(session.as_str())
    )).count()
}

/// Terminated markers, including their byte position within a merged message.
pub(super) fn markers<'a>(
    text: &'a str,
    prefix: &'static str,
) -> impl Iterator<Item = (usize, &'a str)> {
    text.match_indices(prefix).filter_map(move |(at, _)| {
        let rest = &text[at + prefix.len()..];
        let end = rest.find(';')?;
        let root = &rest[..end];
        root.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            .then_some((at, root))
    })
}

impl HistoryChecker for HostAdmission {
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
                    Fact::HostOp {
                        op: HostOp::Send | HostOp::SendBatch,
                        ..
                    }
                )
            })
            .count()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        if self.observed(history) == 0 && history.scenario != "chaos-soak" {
            return Vec::new();
        }
        let deleted = deleted(history);
        let mut named = BTreeMap::new();
        for record in &history.records {
            if let Fact::HostOp {
                op: HostOp::Send | HostOp::SendBatch,
                session,
                roots,
                outcome,
            } = &record.fact
            {
                for root in roots {
                    named.insert((session.as_str(), root.as_str()), (record.at, outcome));
                }
            }
        }
        let mut violations = Vec::new();
        for ((session, root), (at, outcome)) in &named {
            let id = lash_core::PendingTurnInputDraft::keyed_input_id(
                &lash_core::SessionId::from(*session),
                root,
            );
            let rows: Vec<_> = history
                .stores
                .iter()
                .flat_map(|store| &store.inputs)
                .filter(|row| {
                    row.table == "pending_turn_inputs"
                        && row.session == *session
                        && row.id == id.as_str()
                })
                .collect();
            let mut asked = 0;
            let mut answered = 0;
            for transcript in history
                .stores
                .iter()
                .filter_map(|store| store.transcript(session))
            {
                for (role, text) in &transcript.messages {
                    let (prefix, count) = match role.as_str() {
                        "user" => ("input:", &mut asked),
                        "assistant" => ("answer:", &mut answered),
                        _ => continue,
                    };
                    *count += markers(text, prefix)
                        .filter(|(_, found)| found == root)
                        .count();
                }
            }
            let weakened = deleted.contains(session) || root.starts_with("held-");
            let valid = match outcome {
                HostOutcome::Known if !weakened => rows.len() == 1 && asked == 1 && answered == 1,
                HostOutcome::Known | HostOutcome::Maybe => {
                    rows.len() <= 1 && asked <= 1 && answered <= 1
                }
                HostOutcome::Refused { .. } => rows.is_empty() && asked == 0 && answered == 0,
            };
            if !valid {
                let mut violation = Violation::new(INVARIANT, format!(
                    "{session}/{root}: {outcome:?}, {} input row(s), {asked} user marker(s), {answered} answer marker(s); downgraded={weakened}", rows.len()
                )).session(*session).records([*at]);
                for row in rows {
                    violation = violation.row(row.render());
                }
                violations.push(violation);
            }
        }
        let ids: BTreeSet<_> = named
            .keys()
            .map(|(session, root)| {
                (
                    *session,
                    lash_core::PendingTurnInputDraft::keyed_input_id(
                        &lash_core::SessionId::from(*session),
                        root,
                    )
                    .to_string(),
                )
            })
            .collect();
        for store in &history.stores {
            for row in &store.inputs {
                if row.table == "pending_turn_inputs"
                    && !ids.contains(&(row.session.as_str(), row.id.clone()))
                {
                    violations.push(
                        Violation::new(INVARIANT, "stored input has no host request")
                            .session(&row.session)
                            .row(row.render()),
                    );
                }
            }
            for transcript in &store.transcripts {
                for (role, text) in &transcript.messages {
                    let prefix = match role.as_str() {
                        "user" => "input:",
                        "assistant" => "answer:",
                        _ => continue,
                    };
                    for (_, root) in markers(text, prefix) {
                        if !named.contains_key(&(transcript.session.as_str(), root)) {
                            violations.push(
                                Violation::new(INVARIANT, format!("phantom {role} marker {root}"))
                                    .session(&transcript.session)
                                    .row(text),
                            );
                        }
                    }
                }
            }
        }
        violations
    }
}
