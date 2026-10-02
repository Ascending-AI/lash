//! A sequential host's known sends retain their order in committed messages.

use std::collections::BTreeMap;

use super::host_admission::{deleted, markers};
use super::{Fact, History, HistoryChecker, HostOp, HostOutcome, Violation};

pub(super) struct TranscriptOrder;
const INVARIANT: &str = "transcript-order";

impl HistoryChecker for TranscriptOrder {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        let mut previous = BTreeMap::new();
        let deleted = deleted(history);
        let mut pairs = 0;
        for record in &history.records {
            if let Fact::HostOp {
                op: HostOp::Send | HostOp::SendBatch,
                session,
                runs,
                outcome: HostOutcome::Known,
            } = &record.fact
                && !deleted.contains(session.as_str())
                && runs.iter().all(|run| !run.starts_with("held-"))
            {
                pairs += usize::from(previous.insert(session, record.at).is_some());
            }
        }
        pairs
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let deleted = deleted(history);
        let mut violations = Vec::new();
        for store in &history.stores {
            for transcript in &store.transcripts {
                if deleted.contains(transcript.session.as_str()) {
                    continue;
                }
                let mut positions = BTreeMap::new();
                for (message, (role, text)) in transcript.messages.iter().enumerate() {
                    if role == "user" {
                        for (byte, run) in markers(text, "input:") {
                            positions.entry(run).or_insert((message, byte));
                        }
                    }
                }
                let mut prior: Option<(usize, (usize, usize))> = None;
                for record in &history.records {
                    let Fact::HostOp {
                        op: HostOp::Send | HostOp::SendBatch,
                        session,
                        runs,
                        outcome: HostOutcome::Known,
                    } = &record.fact
                    else {
                        continue;
                    };
                    if session != &transcript.session
                        || runs.iter().any(|run| run.starts_with("held-"))
                    {
                        continue;
                    }
                    let found: Vec<_> = runs
                        .iter()
                        .filter_map(|run| positions.get(run.as_str()).copied())
                        .collect();
                    if let (Some((before, last)), Some(first)) = (prior, found.iter().min())
                        && last >= *first
                    {
                        violations.push(
                            Violation::new(
                                INVARIANT,
                                format!(
                                    "known send #{before} follows send #{} in the transcript",
                                    record.at
                                ),
                            )
                            .session(session)
                            .records([before, record.at])
                            .row(format!("{}: {:?}", store.label, transcript.messages)),
                        );
                    }
                    if let Some(last) = found.iter().max() {
                        prior = Some((record.at, *last));
                    }
                }
            }
        }
        violations
    }
}
