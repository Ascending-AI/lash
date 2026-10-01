//! Usage report shapes (ADR 0125).
//!
//! The ledger itself is engine-owned accounting in
//! [`crate::usage_accounting`]; these are the per-attribution views a durable
//! read renders (source, recorded model key and requested wire model), and
//! the reconciliation report a host gets back.

use std::collections::BTreeMap;

use crate::ModelKey;
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

/// What a row of usage is attributed to: the source label, the recorded
/// model key the call ran under and the wire model its request named. Two
/// keys that share a wire model are two attributions.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UsageAttributionKey {
    pub source: String,
    pub model_key: ModelKey,
    pub requested_model: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UsageReportRow {
    pub source: String,
    pub model_key: ModelKey,
    pub requested_model: String,
    pub usage: UsageTotals,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SessionUsageReport {
    pub entry_count: usize,
    /// Whether any counter or canonical total was clamped while producing this
    /// display report. Reads saturate so reporting cannot fail a session.
    pub saturated: bool,
    pub usage: UsageTotals,
    /// Per-source view, derived from `by_attribution`.
    pub by_source: BTreeMap<String, UsageTotals>,
    /// Per-model-key view, derived from `by_attribution`.
    pub by_model_key: BTreeMap<ModelKey, UsageTotals>,
    /// Per-requested-wire-model view, derived from `by_attribution`: keys
    /// that share a wire model fold into one entry here.
    pub by_requested_model: BTreeMap<String, UsageTotals>,
    /// The report's keyed structure: one folded, netted row per
    /// `(source, model_key, requested_model)`. Serialized as a
    /// `UsageReportRow` array.
    #[serde(
        serialize_with = "serialize_by_attribution",
        deserialize_with = "deserialize_by_attribution"
    )]
    pub by_attribution: BTreeMap<UsageAttributionKey, UsageTotals>,
}

fn serialize_by_attribution<S>(
    rows: &BTreeMap<UsageAttributionKey, UsageTotals>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_seq(rows.iter().map(|(attribution, usage)| UsageReportRow {
        source: attribution.source.clone(),
        model_key: attribution.model_key.clone(),
        requested_model: attribution.requested_model.clone(),
        usage: usage.clone(),
    }))
}

fn deserialize_by_attribution<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<UsageAttributionKey, UsageTotals>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let rows = <Vec<UsageReportRow> as serde::Deserialize>::deserialize(deserializer)?;
    // A payload can carry more than one row per key; fold duplicates instead
    // of dropping them.
    let mut saturated = false;
    let mut map = BTreeMap::<UsageAttributionKey, UsageTotals>::new();
    for row in rows {
        map.entry(UsageAttributionKey {
            source: row.source,
            model_key: row.model_key,
            requested_model: row.requested_model,
        })
        .or_default()
        .absorb(&row.usage, &mut saturated);
    }
    Ok(map)
}

impl SessionUsageReport {
    /// The report over already-netted per-attribution totals: the per-source,
    /// per-model-key and per-requested-model views and the overall total are
    /// folded from them.
    pub fn from_attribution_totals(
        entry_count: usize,
        by_attribution: BTreeMap<UsageAttributionKey, UsageTotals>,
    ) -> Self {
        let mut saturated = false;
        let mut usage = UsageTotals::default();
        let mut by_source = BTreeMap::<String, UsageTotals>::new();
        let mut by_model_key = BTreeMap::<ModelKey, UsageTotals>::new();
        let mut by_requested_model = BTreeMap::<String, UsageTotals>::new();
        for (attribution, totals) in &by_attribution {
            usage.absorb(totals, &mut saturated);
            by_source
                .entry(attribution.source.clone())
                .or_default()
                .absorb(totals, &mut saturated);
            by_model_key
                .entry(attribution.model_key.clone())
                .or_default()
                .absorb(totals, &mut saturated);
            by_requested_model
                .entry(attribution.requested_model.clone())
                .or_default()
                .absorb(totals, &mut saturated);
        }
        Self {
            entry_count,
            saturated,
            usage,
            by_source,
            by_model_key,
            by_requested_model,
            by_attribution,
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

/// The usage `after` holds beyond `before`, one row per attribution
/// that grew. Reports only grow, so a key whose counters shrank is an error.
pub fn diff_usage_reports(
    before: &SessionUsageReport,
    after: &SessionUsageReport,
) -> Result<SessionUsageReport, String> {
    let mut rows = BTreeMap::<UsageAttributionKey, UsageTotals>::new();
    for (attribution, after_row) in &after.by_attribution {
        let UsageAttributionKey {
            source,
            model_key,
            requested_model,
        } = attribution;
        let before_row = before
            .by_attribution
            .get(attribution)
            .cloned()
            .unwrap_or_default();
        let subtract = |after: i64, before: i64| match after.checked_sub(before) {
            Some(delta) if delta >= 0 => Ok(delta),
            _ => Err(format!(
                "usage decreased for source/model key/requested model \
                 ({source}, {model_key}, {requested_model})"
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
            format!(
                "usage delta total overflowed for source/model key/requested model \
                 ({source}, {model_key}, {requested_model})"
            )
        })?;
        rows.insert(
            attribution.clone(),
            UsageTotals {
                usage: delta,
                total_tokens,
                unreported_attempts,
                reconciled_attempts,
            },
        );
    }
    for key in before.by_attribution.keys() {
        if !after.by_attribution.contains_key(key) {
            return Err(format!(
                "usage row ({}, {}, {}) disappeared",
                key.source, key.model_key, key.requested_model
            ));
        }
    }
    Ok(SessionUsageReport::from_attribution_totals(
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

    fn attribution(source: &str, model_key: &str, requested_model: &str) -> UsageAttributionKey {
        UsageAttributionKey {
            source: source.to_string(),
            model_key: ModelKey::new(model_key),
            requested_model: requested_model.to_string(),
        }
    }

    fn report(rows: &[(&str, &str, &str, i64, u32)]) -> SessionUsageReport {
        SessionUsageReport::from_attribution_totals(
            rows.len(),
            rows.iter()
                .map(|(source, model_key, requested_model, input, unreported)| {
                    (
                        attribution(source, model_key, requested_model),
                        totals(*input, *unreported),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn a_report_folds_its_views_from_the_keyed_rows() {
        let report = report(&[
            ("turn", "key-a", "a", 3, 1),
            ("turn", "key-b", "b", 4, 0),
            ("direct", "key-a", "a", 5, 0),
        ]);
        assert_eq!(report.usage.usage.input_tokens, 12);
        assert_eq!(report.usage.total_tokens, 12);
        assert_eq!(report.usage.unreported_attempts, 1);
        assert_eq!(report.by_source["turn"].usage.input_tokens, 7);
        assert_eq!(
            report.by_model_key[&ModelKey::new("key-a")]
                .usage
                .input_tokens,
            8
        );
        assert_eq!(report.by_requested_model["a"].usage.input_tokens, 8);
        assert!(!report.saturated);
        let encoded = serde_json::to_value(&report).expect("encode report");
        let decoded: SessionUsageReport = serde_json::from_value(encoded).expect("decode report");
        assert_eq!(decoded, report);
    }

    /// FIG-4405: two keys that share a wire model stay two rows and two
    /// per-key totals; only the per-requested-model view folds them.
    #[test]
    fn two_model_keys_that_share_a_wire_model_are_two_rows() {
        let report = report(&[
            ("turn", "key-a", "shared", 3, 0),
            ("turn", "key-b", "shared", 4, 0),
        ]);
        assert_eq!(report.by_attribution.len(), 2);
        assert_eq!(
            report.by_model_key[&ModelKey::new("key-a")]
                .usage
                .input_tokens,
            3
        );
        assert_eq!(
            report.by_model_key[&ModelKey::new("key-b")]
                .usage
                .input_tokens,
            4
        );
        assert_eq!(report.by_requested_model.len(), 1);
        assert_eq!(report.by_requested_model["shared"].usage.input_tokens, 7);
        let encoded = serde_json::to_value(&report).expect("encode report");
        assert_eq!(encoded["by_attribution"][0]["model_key"], "key-a");
        assert_eq!(encoded["by_attribution"][0]["requested_model"], "shared");
        let decoded: SessionUsageReport = serde_json::from_value(encoded).expect("decode report");
        assert_eq!(decoded, report);
    }

    #[test]
    fn a_diff_keeps_only_the_rows_that_grew_and_refuses_a_shrink() {
        let before = report(&[("turn", "key-a", "a", 3, 0), ("turn", "key-b", "b", 4, 0)]);
        let after = report(&[
            ("turn", "key-a", "a", 3, 0),
            ("turn", "key-b", "b", 9, 1),
            ("direct", "key-a", "a", 2, 0),
        ]);
        let delta = diff_usage_reports(&before, &after).expect("diff");
        assert_eq!(delta.by_attribution.len(), 2);
        assert_eq!(
            delta.by_attribution[&attribution("turn", "key-b", "b")],
            totals(5, 1)
        );
        assert_eq!(delta.usage.usage.input_tokens, 7);
        assert!(diff_usage_reports(&after, &before).is_err());
    }
}
