//! Token usage accounting: ledger entries, usage totals, reports, and diff helpers.
//!
//! Extracted from `runtime/mod.rs` as part of the runtime split. All items
//! keep their original public paths via `pub use` in `mod.rs` — no API
//! changes.

use std::collections::{BTreeMap, HashMap};

use crate::session_model::TokenUsage;
use lash_sansio::PromptUsage;

/// A single row in the token cost ledger. One per unique
/// `(source, model)` pair — accumulated, not per-call.
///
/// Its semantic fields are projected by Lash's versioned usage-payload
/// identity encoder. Adding or changing a field here or in nested
/// [`TokenUsage`] requires an encoding version bump and replacement golden
/// corpus; the serde representation itself is deliberately not the identity
/// format.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenLedgerEntry {
    /// Caller-supplied label: `"turn"`, `"subagent"`, `"compaction"`,
    /// `"observer"`, `"reflector"`, or any plugin-defined
    /// string. Core treats the value as an opaque grouping key.
    pub source: String,
    /// Model identifier used for the LLM call (e.g.
    /// `"anthropic/claude-haiku-4-5"`).
    pub model: String,
    /// Accumulated token counts for this `(source, model)` pair.
    pub usage: TokenUsage,
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
}

impl UsageTotals {
    fn from_usage(usage: &TokenUsage, saturated: &mut bool) -> Self {
        let (total_tokens, total_saturated) = saturating_usage_total(usage);
        *saturated |= total_saturated;
        Self {
            usage: usage.clone(),
            total_tokens,
        }
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
    /// display report. Durable commit and load paths remain strict and return a
    /// typed error instead; reads saturate so reporting cannot fail a session.
    pub saturated: bool,
    pub usage: UsageTotals,
    pub by_source: BTreeMap<String, UsageTotals>,
    pub by_model: BTreeMap<String, UsageTotals>,
    pub by_source_model: Vec<UsageReportRow>,
}

impl SessionUsageReport {
    pub fn from_entries(entries: &[TokenLedgerEntry]) -> Self {
        Self::from_entries_with_saturation(entries, false)
    }

    pub(super) fn from_entries_with_saturation(
        entries: &[TokenLedgerEntry],
        mut saturated: bool,
    ) -> Self {
        let mut total = TokenUsage::default();
        let mut by_source_usage = BTreeMap::<String, TokenUsage>::new();
        let mut by_model_usage = BTreeMap::<String, TokenUsage>::new();
        let mut by_source_model = Vec::with_capacity(entries.len());

        for entry in entries {
            saturated |= saturating_add_usage(&mut total, &entry.usage);
            saturated |= saturating_add_usage(
                by_source_usage.entry(entry.source.clone()).or_default(),
                &entry.usage,
            );
            saturated |= saturating_add_usage(
                by_model_usage.entry(entry.model.clone()).or_default(),
                &entry.usage,
            );
            by_source_model.push(UsageReportRow {
                source: entry.source.clone(),
                model: entry.model.clone(),
                usage: UsageTotals::from_usage(&entry.usage, &mut saturated),
            });
        }

        let usage = UsageTotals::from_usage(&total, &mut saturated);
        let by_source = by_source_usage
            .into_iter()
            .map(|(key, usage)| (key, UsageTotals::from_usage(&usage, &mut saturated)))
            .collect();
        let by_model = by_model_usage
            .into_iter()
            .map(|(key, usage)| (key, UsageTotals::from_usage(&usage, &mut saturated)))
            .collect();

        Self {
            entry_count: entries.len(),
            saturated,
            usage,
            by_source,
            by_model,
            by_source_model,
        }
    }
}

fn saturating_add_usage(target: &mut TokenUsage, incoming: &TokenUsage) -> bool {
    let mut saturated = false;
    macro_rules! add_counter {
        ($field:ident) => {
            target.$field = match target.$field.checked_add(incoming.$field) {
                Some(value) => value,
                None => {
                    saturated = true;
                    target.$field.saturating_add(incoming.$field)
                }
            };
        };
    }
    add_counter!(input_tokens);
    add_counter!(output_tokens);
    add_counter!(cache_read_input_tokens);
    add_counter!(cache_write_input_tokens);
    add_counter!(reasoning_output_tokens);
    saturated
}

fn saturating_usage_total(usage: &TokenUsage) -> (i64, bool) {
    let mut saturated = false;
    let total = [
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_input_tokens,
        usage.cache_write_input_tokens,
    ]
    .into_iter()
    .fold(0_i64, |total, counter| {
        total.checked_add(counter).unwrap_or_else(|| {
            saturated = true;
            total.saturating_add(counter)
        })
    });
    (total, saturated)
}

pub fn diff_token_ledger(
    before: &[TokenLedgerEntry],
    after: &[TokenLedgerEntry],
) -> Result<Vec<TokenLedgerEntry>, String> {
    let before_index = before
        .iter()
        .map(|entry| ((entry.source.as_str(), entry.model.as_str()), &entry.usage))
        .collect::<HashMap<_, _>>();
    let after_index = after
        .iter()
        .map(|entry| ((entry.source.as_str(), entry.model.as_str()), &entry.usage))
        .collect::<HashMap<_, _>>();

    let mut keys = before_index
        .keys()
        .copied()
        .chain(after_index.keys().copied())
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();

    let mut out = Vec::new();
    for (source, model) in keys {
        let before_usage = before_index
            .get(&(source, model))
            .copied()
            .cloned()
            .unwrap_or_default();
        let after_usage = after_index
            .get(&(source, model))
            .copied()
            .cloned()
            .unwrap_or_default();
        let subtract = |after: i64, before: i64| {
            after.checked_sub(before).ok_or_else(|| {
                format!("token ledger delta overflowed for source/model ({source}, {model})")
            })
        };
        let delta = TokenUsage {
            input_tokens: subtract(after_usage.input_tokens, before_usage.input_tokens)?,
            output_tokens: subtract(after_usage.output_tokens, before_usage.output_tokens)?,
            cache_read_input_tokens: subtract(
                after_usage.cache_read_input_tokens,
                before_usage.cache_read_input_tokens,
            )?,
            cache_write_input_tokens: subtract(
                after_usage.cache_write_input_tokens,
                before_usage.cache_write_input_tokens,
            )?,
            reasoning_output_tokens: subtract(
                after_usage.reasoning_output_tokens,
                before_usage.reasoning_output_tokens,
            )?,
        };
        if delta.input_tokens < 0
            || delta.output_tokens < 0
            || delta.cache_read_input_tokens < 0
            || delta.cache_write_input_tokens < 0
            || delta.reasoning_output_tokens < 0
        {
            return Err(format!(
                "token ledger decreased for source/model ({source}, {model})"
            ));
        }
        if delta.is_zero() {
            continue;
        }
        out.push(TokenLedgerEntry {
            source: source.to_string(),
            model: model.to_string(),
            usage: delta,
        });
    }
    Ok(out)
}

pub fn diff_usage_reports(
    before: &SessionUsageReport,
    after: &SessionUsageReport,
) -> Result<Vec<TokenLedgerEntry>, String> {
    let row_entries = |report: &SessionUsageReport| {
        report
            .by_source_model
            .iter()
            .map(|row| TokenLedgerEntry {
                source: row.source.clone(),
                model: row.model.clone(),
                usage: row.usage.usage.clone(),
            })
            .collect::<Vec<_>>()
    };
    diff_token_ledger(&row_entries(before), &row_entries(after))
}

pub(super) fn merge_ledger_entry_saturating(
    ledger: &mut Vec<TokenLedgerEntry>,
    entry: TokenLedgerEntry,
) -> bool {
    if entry.usage.is_zero() {
        return false;
    }
    if let Some(existing) = ledger
        .iter_mut()
        .find(|e| e.source == entry.source && e.model == entry.model)
    {
        saturating_add_usage(&mut existing.usage, &entry.usage)
    } else {
        ledger.push(entry);
        false
    }
}

pub(super) fn normalize_prompt_usage(usage: &TokenUsage) -> Option<PromptUsage> {
    let input_tokens = usage.input_tokens.max(0) as usize;
    let output_tokens = usage.output_tokens.max(0) as usize;
    let cache_read_input_tokens = usage.cache_read_input_tokens.max(0) as usize;
    let cache_write_input_tokens = usage.cache_write_input_tokens.max(0) as usize;
    if input_tokens == 0
        && cache_read_input_tokens == 0
        && cache_write_input_tokens == 0
        && output_tokens == 0
    {
        return None;
    }

    let prompt_context_tokens = input_tokens
        .saturating_add(cache_read_input_tokens)
        .saturating_add(cache_write_input_tokens);
    let context_budget_tokens = input_tokens
        .saturating_add(output_tokens)
        .saturating_add(cache_read_input_tokens)
        .saturating_add(cache_write_input_tokens);

    Some(PromptUsage {
        prompt_context_tokens,
        input_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        context_budget_tokens,
    })
}
