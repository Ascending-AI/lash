//! Usage report shapes (ADR 0125).
//!
//! The ledger itself is engine-owned accounting in
//! [`crate::usage_accounting`]; these are the per-`(source, model)` views a
//! durable read renders, and the reconciliation report a host gets back.

use std::collections::BTreeMap;

use crate::session_model::TokenUsage;
use crate::usage_accounting::OutstandingUsageAttempt;

/// Outcome of one host-invoked `LashRuntime::reconcile_unreported_usage`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsageReconciliationReport {
    /// Attempts whose usage the provider recovered; each produced one
    /// correction fact.
    pub reconciled: Vec<ReconciledUsageAttempt>,
    /// Attempts still open: no generation id, the provider has no record, the
    /// bounded lookup failed, or the correction conflicts with a stored one.
    /// They stay outstanding for a later call.
    pub unresolved: Vec<OutstandingUsageAttempt>,
}

/// One attempt whose usage was recovered after the fact.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReconciledUsageAttempt {
    pub attempt: OutstandingUsageAttempt,
    /// Recovered counters, as appended to the ledger.
    pub usage: TokenUsage,
    /// The provider's own accounting record for the generation.
    pub provider_usage: serde_json::Value,
}

/// Aggregated usage for a report row: the canonical [`TokenUsage`] counters
/// plus a precomputed `total_tokens` so JSON consumers don't recompute the sum.
/// `TokenUsage` is embedded (flattened) rather than re-declared so a new counter
/// tier is added in exactly one place and automatically flows through here.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UsageTotals {
    #[serde(flatten)]
    pub usage: TokenUsage,
    pub total_tokens: i64,
    /// Interrupted attempts whose provider usage never arrived and has not
    /// been reconciled: the calls the counters above do not cover. A host
    /// summing cost treats a nonzero value as an incomplete sum.
    #[serde(default, skip_serializing_if = "count_is_zero")]
    pub unreported_attempts: u32,
    /// Previously unreported attempts whose usage was recovered from the
    /// provider by `LashRuntime::reconcile_unreported_usage`;
    /// their counters are included above.
    #[serde(default, skip_serializing_if = "count_is_zero")]
    pub reconciled_attempts: u32,
}

fn count_is_zero(count: &u32) -> bool {
    *count == 0
}

impl UsageTotals {
    /// Fold one netted totals row into another.
    fn absorb(&mut self, incoming: &UsageTotals, saturated: &mut bool) {
        *saturated |= saturating_add_usage(&mut self.usage, &incoming.usage);
        self.total_tokens = match self.total_tokens.checked_add(incoming.total_tokens) {
            Some(total) => total,
            None => {
                *saturated = true;
                self.total_tokens.saturating_add(incoming.total_tokens)
            }
        };
        self.unreported_attempts = self
            .unreported_attempts
            .saturating_add(incoming.unreported_attempts);
        self.reconciled_attempts = self
            .reconciled_attempts
            .saturating_add(incoming.reconciled_attempts);
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UsageReportRow {
    pub source: String,
    pub model: String,
    pub usage: UsageTotals,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SessionUsageReport {
    pub entry_count: usize,
    /// Whether any counter or canonical total was clamped while producing this
    /// display report. Reads saturate so reporting cannot fail a session.
    pub saturated: bool,
    pub usage: UsageTotals,
    /// Per-source view, derived from `by_source_model`.
    pub by_source: BTreeMap<String, UsageTotals>,
    /// Per-model view, derived from `by_source_model`.
    pub by_model: BTreeMap<String, UsageTotals>,
    /// The report's keyed structure: one folded, netted row per
    /// `(source, model)` pair. Serialized as a `UsageReportRow` array.
    #[serde(
        serialize_with = "serialize_by_source_model",
        deserialize_with = "deserialize_by_source_model"
    )]
    pub by_source_model: BTreeMap<(String, String), UsageTotals>,
}

fn serialize_by_source_model<S>(
    rows: &BTreeMap<(String, String), UsageTotals>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_seq(rows.iter().map(|((source, model), usage)| UsageReportRow {
        source: source.clone(),
        model: model.clone(),
        usage: usage.clone(),
    }))
}

fn deserialize_by_source_model<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<(String, String), UsageTotals>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let rows = <Vec<UsageReportRow> as serde::Deserialize>::deserialize(deserializer)?;
    // A payload can carry more than one row per key; fold duplicates instead
    // of dropping them.
    let mut saturated = false;
    let mut map = BTreeMap::<(String, String), UsageTotals>::new();
    for row in rows {
        map.entry((row.source, row.model))
            .or_default()
            .absorb(&row.usage, &mut saturated);
    }
    Ok(map)
}

impl SessionUsageReport {
    /// The report over already-netted `(source, model)` totals: the per-source
    /// and per-model views and the overall total are folded from them.
    pub fn from_source_model_totals(
        entry_count: usize,
        by_source_model: BTreeMap<(String, String), UsageTotals>,
    ) -> Self {
        let mut saturated = false;
        let mut usage = UsageTotals::default();
        let mut by_source = BTreeMap::<String, UsageTotals>::new();
        let mut by_model = BTreeMap::<String, UsageTotals>::new();
        for ((source, model), totals) in &by_source_model {
            usage.absorb(totals, &mut saturated);
            by_source
                .entry(source.clone())
                .or_default()
                .absorb(totals, &mut saturated);
            by_model
                .entry(model.clone())
                .or_default()
                .absorb(totals, &mut saturated);
        }
        Self {
            entry_count,
            saturated,
            usage,
            by_source,
            by_model,
            by_source_model,
        }
    }
}

fn saturating_add_usage(target: &mut TokenUsage, incoming: &TokenUsage) -> bool {
    let (merged, saturated) = target.saturating_add(incoming);
    *target = merged;
    saturated
}

fn usage_total(usage: &TokenUsage) -> Option<i64> {
    [
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_input_tokens,
        usage.cache_write_input_tokens,
    ]
    .into_iter()
    .try_fold(0_i64, i64::checked_add)
}

/// The usage `after` holds beyond `before`, one row per `(source, model)` key
/// that grew. Reports only grow, so a key whose counters shrank is an error.
pub fn diff_usage_reports(
    before: &SessionUsageReport,
    after: &SessionUsageReport,
) -> Result<SessionUsageReport, String> {
    let mut rows = BTreeMap::<(String, String), UsageTotals>::new();
    for ((source, model), after_row) in &after.by_source_model {
        let before_row = before
            .by_source_model
            .get(&(source.clone(), model.clone()))
            .cloned()
            .unwrap_or_default();
        let subtract = |after: i64, before: i64| match after.checked_sub(before) {
            Some(delta) if delta >= 0 => Ok(delta),
            _ => Err(format!(
                "usage decreased for source/model ({source}, {model})"
            )),
        };
        let delta = TokenUsage {
            input_tokens: subtract(after_row.usage.input_tokens, before_row.usage.input_tokens)?,
            output_tokens: subtract(
                after_row.usage.output_tokens,
                before_row.usage.output_tokens,
            )?,
            cache_read_input_tokens: subtract(
                after_row.usage.cache_read_input_tokens,
                before_row.usage.cache_read_input_tokens,
            )?,
            cache_write_input_tokens: subtract(
                after_row.usage.cache_write_input_tokens,
                before_row.usage.cache_write_input_tokens,
            )?,
            reasoning_output_tokens: subtract(
                after_row.usage.reasoning_output_tokens,
                before_row.usage.reasoning_output_tokens,
            )?,
        };
        let unreported_attempts = after_row
            .unreported_attempts
            .saturating_sub(before_row.unreported_attempts);
        let reconciled_attempts = after_row
            .reconciled_attempts
            .saturating_sub(before_row.reconciled_attempts);
        if delta.is_zero() && unreported_attempts == 0 && reconciled_attempts == 0 {
            continue;
        }
        let total_tokens = usage_total(&delta).ok_or_else(|| {
            format!("usage delta total overflowed for source/model ({source}, {model})")
        })?;
        rows.insert(
            (source.clone(), model.clone()),
            UsageTotals {
                usage: delta,
                total_tokens,
                unreported_attempts,
                reconciled_attempts,
            },
        );
    }
    for key in before.by_source_model.keys() {
        if !after.by_source_model.contains_key(key) {
            return Err(format!("usage row ({}, {}) disappeared", key.0, key.1));
        }
    }
    Ok(SessionUsageReport::from_source_model_totals(
        after.entry_count.saturating_sub(before.entry_count),
        rows,
    ))
}

/// The last call's usage, `Some` only when the call reported any nonzero
/// counter. A fully zeroed report carries no prompt-side information and is
/// stored as `None`, the same as no completed call.
pub fn nonzero_usage(usage: TokenUsage) -> Option<TokenUsage> {
    (!usage.is_zero()).then_some(usage)
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn totals(input_tokens: i64, unreported_attempts: u32) -> UsageTotals {
        UsageTotals {
            usage: TokenUsage {
                input_tokens,
                ..TokenUsage::default()
            },
            total_tokens: input_tokens,
            unreported_attempts,
            reconciled_attempts: 0,
        }
    }

    fn report(rows: &[(&str, &str, i64, u32)]) -> SessionUsageReport {
        SessionUsageReport::from_source_model_totals(
            rows.len(),
            rows.iter()
                .map(|(source, model, input, unreported)| {
                    (
                        ((*source).to_string(), (*model).to_string()),
                        totals(*input, *unreported),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn a_report_folds_its_views_from_the_keyed_rows() {
        let report = report(&[
            ("turn", "a", 3, 1),
            ("turn", "b", 4, 0),
            ("direct", "a", 5, 0),
        ]);
        assert_eq!(report.usage.usage.input_tokens, 12);
        assert_eq!(report.usage.total_tokens, 12);
        assert_eq!(report.usage.unreported_attempts, 1);
        assert_eq!(report.by_source["turn"].usage.input_tokens, 7);
        assert_eq!(report.by_model["a"].usage.input_tokens, 8);
        assert!(!report.saturated);
        let encoded = serde_json::to_value(&report).expect("encode report");
        let decoded: SessionUsageReport = serde_json::from_value(encoded).expect("decode report");
        assert_eq!(decoded, report);
    }

    #[test]
    fn a_diff_keeps_only_the_rows_that_grew_and_refuses_a_shrink() {
        let before = report(&[("turn", "a", 3, 0), ("turn", "b", 4, 0)]);
        let after = report(&[
            ("turn", "a", 3, 0),
            ("turn", "b", 9, 1),
            ("direct", "a", 2, 0),
        ]);
        let delta = diff_usage_reports(&before, &after).expect("diff");
        assert_eq!(delta.by_source_model.len(), 2);
        assert_eq!(
            delta.by_source_model[&("turn".to_string(), "b".to_string())],
            totals(5, 1)
        );
        assert_eq!(delta.usage.usage.input_tokens, 7);
        assert!(diff_usage_reports(&after, &before).is_err());
    }
}
