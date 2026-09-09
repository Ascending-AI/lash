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
    /// Whether `usage` is provider-reported, a typed hole left by attempts
    /// whose usage never arrived, or a host-invoked correction. Rows written
    /// before this field existed decode as
    /// [`LedgerUsageDisposition::Reported`].
    #[serde(default, skip_serializing_if = "LedgerUsageDisposition::is_reported")]
    pub usage_disposition: LedgerUsageDisposition,
}

impl TokenLedgerEntry {
    /// A provider-reported row: the ordinary accumulated `(source, model)` pair.
    pub fn reported(
        source: impl Into<String>,
        model: impl Into<String>,
        usage: TokenUsage,
    ) -> Self {
        Self {
            source: source.into(),
            model: model.into(),
            usage,
            usage_disposition: LedgerUsageDisposition::Reported,
        }
    }
}

/// One interrupted attempt whose usage never arrived, kept runtime-resident
/// until a host reconciles it (FIG-2765).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnreportedUsageAttempt {
    /// The sealed call the attempt belongs to.
    pub call_id: String,
    /// The attempt within that call.
    pub attempt_ordinal: u32,
    /// Ledger source the hole was written under (`"turn"`).
    pub source: String,
    /// Model the attempt was billed against.
    pub model: String,
    /// Provider generation id (the response id the first stream chunk
    /// carried), when the attempt got far enough to have one. Without it the
    /// attempt cannot be reconciled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
}

/// Outcome of one host-invoked [`crate::runtime::LashRuntime::reconcile_unreported_usage`].
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsageReconciliationReport {
    /// Attempts whose usage the provider recovered; each produced one
    /// `Reconciled` ledger correction row.
    pub reconciled: Vec<ReconciledUsageAttempt>,
    /// Attempts still open: no generation id, the provider has no record, or
    /// the bounded lookup failed. They stay registered for a later call.
    pub unresolved: Vec<UnreportedUsageAttempt>,
}

/// One attempt whose usage was recovered after the fact.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReconciledUsageAttempt {
    pub attempt: UnreportedUsageAttempt,
    /// Recovered counters, as appended to the ledger.
    pub usage: TokenUsage,
    /// The provider's own accounting record for the generation.
    pub provider_usage: serde_json::Value,
}

/// How one ledger row relates to provider-reported usage.
///
/// ADR 0031: absence means unreported and an explicit zero is information. A
/// turn whose attempt ended before the provider's usage arrived writes an
/// `Unreported` row — zero counters, a nonzero attempt count — so a host
/// summing cost sees the hole instead of a silent zero. A later host-invoked
/// reconciliation appends a `Reconciled` correction row attributed to the
/// original attempt; rows are never rewritten.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerUsageDisposition {
    /// Provider-reported usage, accumulated per `(source, model)`.
    #[default]
    Reported,
    /// Attempts that ended without provider usage. `usage` is zero; the count
    /// is the number of billed-but-uncounted calls folded into this row.
    Unreported {
        /// Interrupted attempts whose usage never arrived.
        attempts: u32,
    },
    /// Usage recovered after the fact from the provider's generation record
    /// for one previously unreported attempt. Never merged with other rows.
    Reconciled {
        /// The sealed call this correction belongs to.
        call_id: String,
        /// The attempt within that call.
        attempt_ordinal: u32,
    },
}

impl LedgerUsageDisposition {
    pub fn is_reported(&self) -> bool {
        matches!(self, Self::Reported)
    }

    /// Whether a row carrying `self` may absorb a row carrying `other`.
    /// Reported and unreported rows accumulate with their own kind;
    /// reconciled corrections stay one row per attempt.
    pub fn accumulates_with(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Reported, Self::Reported) | (Self::Unreported { .. }, Self::Unreported { .. })
        )
    }

    /// Interrupted attempts this row stands for that still have no usage.
    pub fn unreported_attempts(&self) -> u32 {
        match self {
            Self::Unreported { attempts } => *attempts,
            Self::Reported | Self::Reconciled { .. } => 0,
        }
    }

    /// Corrections this row stands for.
    pub fn reconciled_attempts(&self) -> u32 {
        match self {
            Self::Reconciled { .. } => 1,
            Self::Reported | Self::Unreported { .. } => 0,
        }
    }

    /// A row that carries no usage and stands for no attempt: nothing to
    /// record. Unreported rows are never empty — the hole is the content.
    pub(crate) fn row_is_empty(&self, usage: &TokenUsage) -> bool {
        usage.is_zero() && self.unreported_attempts() == 0 && self.reconciled_attempts() == 0
    }

    /// Fold `other` into `self` for an accumulating pair; returns whether the
    /// attempt count clamped.
    pub(crate) fn absorb_saturating(&mut self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unreported { attempts }, Self::Unreported { attempts: incoming }) => {
                let (next, overflowed) = attempts.overflowing_add(*incoming);
                *attempts = if overflowed { u32::MAX } else { next };
                overflowed
            }
            _ => false,
        }
    }
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
    /// provider by [`crate::runtime::LashRuntime::reconcile_unreported_usage`];
    /// their counters are included above.
    #[serde(default, skip_serializing_if = "count_is_zero")]
    pub reconciled_attempts: u32,
}

fn count_is_zero(count: &u32) -> bool {
    *count == 0
}

/// Per-key accumulator behind a report row: counters plus the attempt counts
/// that say how complete those counters are.
#[derive(Clone, Debug, Default)]
struct UsageAccumulator {
    usage: TokenUsage,
    unreported_attempts: u32,
    reconciled_attempts: u32,
}

impl UsageAccumulator {
    fn add(&mut self, entry: &TokenLedgerEntry) -> bool {
        let saturated = saturating_add_usage(&mut self.usage, &entry.usage);
        self.unreported_attempts = self
            .unreported_attempts
            .saturating_add(entry.usage_disposition.unreported_attempts());
        self.reconciled_attempts = self
            .reconciled_attempts
            .saturating_add(entry.usage_disposition.reconciled_attempts());
        saturated
    }
}

impl UsageTotals {
    fn from_accumulator(accumulator: &UsageAccumulator, saturated: &mut bool) -> Self {
        let (total_tokens, total_saturated) = saturating_usage_total(&accumulator.usage);
        *saturated |= total_saturated;
        Self {
            usage: accumulator.usage.clone(),
            total_tokens,
            // Corrections are append-only rows, so the outstanding hole is
            // derived: every reconciliation fills exactly one unreported
            // attempt.
            unreported_attempts: accumulator
                .unreported_attempts
                .saturating_sub(accumulator.reconciled_attempts),
            reconciled_attempts: accumulator.reconciled_attempts,
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
        let mut total = UsageAccumulator::default();
        let mut by_source_usage = BTreeMap::<String, UsageAccumulator>::new();
        let mut by_model_usage = BTreeMap::<String, UsageAccumulator>::new();
        let mut by_source_model = Vec::with_capacity(entries.len());

        for entry in entries {
            saturated |= total.add(entry);
            saturated |= by_source_usage
                .entry(entry.source.clone())
                .or_default()
                .add(entry);
            saturated |= by_model_usage
                .entry(entry.model.clone())
                .or_default()
                .add(entry);
            let mut row = UsageAccumulator::default();
            saturated |= row.add(entry);
            by_source_model.push(UsageReportRow {
                source: entry.source.clone(),
                model: entry.model.clone(),
                usage: UsageTotals::from_accumulator(&row, &mut saturated),
            });
        }

        let usage = UsageTotals::from_accumulator(&total, &mut saturated);
        let by_source = by_source_usage
            .into_iter()
            .map(|(key, usage)| (key, UsageTotals::from_accumulator(&usage, &mut saturated)))
            .collect();
        let by_model = by_model_usage
            .into_iter()
            .map(|(key, usage)| (key, UsageTotals::from_accumulator(&usage, &mut saturated)))
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
    // Reconciled corrections share a key with the reported row they fix, so
    // the diff folds every row of a key before subtracting.
    let index = |entries: &[TokenLedgerEntry]| {
        let mut index = HashMap::<(String, String), TokenUsage>::new();
        for entry in entries {
            let key = (entry.source.clone(), entry.model.clone());
            let folded = index.entry(key).or_default();
            *folded = folded.checked_add(&entry.usage).map_err(|overflow| {
                format!(
                    "token ledger {} overflowed for source/model ({}, {})",
                    overflow.counter(),
                    entry.source,
                    entry.model
                )
            })?;
        }
        Ok::<_, String>(index)
    };
    let before_index = index(before)?;
    let after_index = index(after)?;

    let mut keys = before_index
        .keys()
        .chain(after_index.keys())
        .cloned()
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();

    let mut out = Vec::new();
    for key in keys {
        let (source, model) = (key.0.as_str(), key.1.as_str());
        let before_usage = before_index.get(&key).cloned().unwrap_or_default();
        let after_usage = after_index.get(&key).cloned().unwrap_or_default();
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
        out.push(TokenLedgerEntry::reported(source, model, delta));
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
            .map(|row| {
                TokenLedgerEntry::reported(
                    row.source.clone(),
                    row.model.clone(),
                    row.usage.usage.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    diff_token_ledger(&row_entries(before), &row_entries(after))
}

pub(super) fn merge_ledger_entry_saturating(
    ledger: &mut Vec<TokenLedgerEntry>,
    entry: TokenLedgerEntry,
) -> bool {
    if entry.usage_disposition.row_is_empty(&entry.usage) {
        return false;
    }
    if let Some(existing) = ledger.iter_mut().find(|e| {
        e.source == entry.source
            && e.model == entry.model
            && e.usage_disposition
                .accumulates_with(&entry.usage_disposition)
    }) {
        let saturated = saturating_add_usage(&mut existing.usage, &entry.usage);
        existing
            .usage_disposition
            .absorb_saturating(&entry.usage_disposition)
            | saturated
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
