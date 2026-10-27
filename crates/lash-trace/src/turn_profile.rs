//! Content-free physical work for one owner attempt of a turn.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bounded phases. Intervals overlap when one phase calls another; consumers
/// must use the deepest active interval for an exclusive wall-time table.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnProfile {
    pub elapsed_ns: u64,
    pub started_at_unix_ns: u64,
    pub phases: Vec<TurnPhaseInterval>,
    pub sql: Vec<TurnSqlWork>,
    pub writes: Vec<TurnStoreWrite>,
    pub statements: u64,
    pub dml: u64,
    pub transactions: u64,
    /// Newly installed canonical graph bytes, not WAL or filesystem bytes.
    pub store_bytes_written: u64,
    pub snapshots: u64,
    pub manifests: u64,
    pub link_payloads: u64,
    pub omitted_intervals: u64,
    pub omitted_sql_keys: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnPhaseInterval {
    pub phase: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub depth: u32,
    pub revision: Option<i64>,
    /// SQL directly executed in this interval, excluding nested intervals.
    pub statements: u64,
    pub round_trips: u64,
    pub rows_returned: u64,
    pub rows_affected: u64,
    pub bound_bytes: u64,
    pub result_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnSqlWork {
    /// Index of the exact deepest interval, so repeated phase names stay distinct.
    pub phase_index: Option<usize>,
    pub phase: String,
    pub origin: String,
    pub shape: String,
    pub backend: String,
    pub owner_commit: bool,
    pub revision: Option<i64>,
    pub statements: u64,
    pub round_trips: u64,
    pub rows_returned: u64,
    pub rows_affected: u64,
    pub bound_bytes: u64,
    pub result_bytes: u64,
}

impl TurnProfile {
    /// The phase ledger's published spans, derived from the terminal record's
    /// id and interval order. Every sink can recover the same identities and
    /// inherited sampling decision without retaining exporter state.
    pub fn phase_positions<'a>(
        &'a self,
        record: &'a str,
        parent: &'a crate::TracePosition,
    ) -> impl Iterator<Item = (&'a TurnPhaseInterval, crate::TracePosition)> + 'a {
        self.phases.iter().enumerate().map(move |(index, phase)| {
            let at = |offset| {
                crate::TraceTimestamp::from_unix_ms(
                    self.started_at_unix_ns.saturating_add(offset) / 1_000_000,
                )
            };
            let position = crate::TracePosition {
                span: crate::telemetry::derive_turn_phase_span(&parent.span, record, index),
                parent: Some(parent.span.clone()),
                links: crate::TraceLinks::new(),
                role: crate::SpanRole::Whole,
                kind: crate::TraceSpanKind::Agent,
                started_at: at(phase.start_ns),
                ended_at: Some(at(phase.end_ns)),
                scope: parent.scope.clone(),
                attempt: parent.attempt.clone(),
            };
            (phase, position)
        })
    }

    /// Disjoint critical-path shares, including time outside named phases.
    pub fn exclusive_ns(&self) -> BTreeMap<String, u64> {
        let mut cuts = vec![0, self.elapsed_ns];
        for phase in &self.phases {
            cuts.extend([
                phase.start_ns.min(self.elapsed_ns),
                phase.end_ns.min(self.elapsed_ns),
            ]);
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut result = BTreeMap::new();
        for cut in cuts.windows(2) {
            let phase = self
                .phases
                .iter()
                .filter(|p| p.start_ns <= cut[0] && p.end_ns >= cut[1])
                .max_by_key(|p| p.depth);
            let key = phase.map_or_else(
                || "owner orchestration".to_owned(),
                |p| match p.revision {
                    Some(revision) => format!("{} revision {revision}", p.phase),
                    None => p.phase.clone(),
                },
            );
            *result.entry(key).or_default() += cut[1] - cut[0];
        }
        result
    }
}

/// Canonical graph nodes physically inserted in an attempt, committed on success.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnStoreWrite {
    pub phase: String,
    pub revision: Option<i64>,
    pub kind: String,
    pub nodes: u64,
    pub bytes: u64,
}
